# Research: one module per feature

> Archived on 2026-09-29: this completed implementation history is
> superseded by the current [architecture](../ARCHITECTURE.md) and
> [active handoff](../HANDOFF.md).

> The UI half of this (each feature's `ui.rs` next to its plugin, behind a
> `UiPlugin` seam) was replaced on 2026-09-26: all UI code now lives in
> `src/ui/`. See [`adr/0001`](../adr/0001-native-ui-in-iced.md).

Status: done (2026-09-25). Every phase is implemented: 0 (ping, §7), 0b
(find my phone), 1 (battery), 2 (clipboard), 3 (share), 4 (browse) and 5
(core cleanup). The shape as built is described in
[`ARCHITECTURE.md`](../ARCHITECTURE.md) §2, which is the source of truth;
this file is the history behind it: the reasoning, the plan, and what
each phase found (§8). Paths and type names below are as they were when
each part was written (`application/` is now `core/`, `ApplicationHandle`
is `Core`).

Question: how should the daemon be split so that a feature (ping,
battery, clipboard, share, browsing) lives in one module that plugs into a
small core, instead of being spread over the central service?

## 1. Goal and non-goals

Set by the owner:

- **Goal: a codebase that scales.** Specifically:
  - agents working in parallel worktrees stop conflicting in the same
    central files;
  - adding a feature stops meaning edits to about eight places;
  - a feature can be read and tested on its own.
- **Non-goals:** loading plugins at runtime, a plugin marketplace, a stable
  plugin ABI, a crate per feature. It stays one crate, with boundaries kept
  by module visibility and review.
- **Scope:** the Rust daemon. `client.rs`, the CLI and the Flutter UI keep
  their shape for now, though they follow wire changes.
- **Wire contract:** free to change where that makes the design cleaner.
  The pilot didn't need any change; phase 1 moved `battery` under
  `plugins` (§5.5), phase 2 moved `clipboardSyncEnabled` to
  `plugins.clipboard.syncEnabled` (§5.6), and phase 3 moved sending a file
  from `POST /transfers` to `POST /devices/{id}/share` (§8). Phase 4
  needed no change.

## 2. Where we are

A feature today is written in these places:

| Place | What goes there |
| --- | --- |
| `plugins/<name>.rs` | packet bodies and builders (this part is already per feature) |
| `plugins/mod.rs` | a `capabilities()` entry, an `IncomingPluginPacket` variant, a `dispatch_incoming` arm |
| `application/service.rs` | the logic, a `handle_peer_packet` arm, `ApplicationHandle` methods, `ApplicationService` trait methods and the forwarding impl |
| `application/state.rs` | its state inside the one `ApplicationState`, its snapshot types, `Query`/`QueryResult` variants |
| `application/events.rs` | `EventData` variants and their `event_type` arms |
| `ApplicationError` | its error variants |
| `api.rs` | routes, handlers, `map_error` arms, and path-sniffing in the body-limit middleware for streaming uploads |
| `device.rs` | fields it adds to `DeviceSnapshot` (battery) |
| `application/settings.rs`, `config/settings.rs` | its settings (`clipboardSyncEnabled`) |
| lifecycle code in `service.rs` | cleanup in `register_connection`, `unregister_connection`, `forget_device`, `handle_peer_unpair`, `RunningService::shutdown` |

The numbers bear this out: `service.rs` is 3,192 lines, a third of the
library, and each of the last three Rust features (#11 battery, #13
offline devices, #14 clipboard send) changed it by 84–172 lines.
`ApplicationService` has a single implementation. Even `tests/api.rs`
drives the real `ApplicationHandle`, so the trait isn't a seam, just one
more central list.

`plugins/mod.rs` and ARCHITECTURE §2 say the fixed table is deliberate,
chosen for the MVP ("not a plugin marketplace"). That reasoning still
holds against *dynamic* plugins. This proposal keeps the set fixed at
compile time and changes only where each feature's code lives.

## 3. The idea, and where I push back or go further

The proposal: expose core components, and have each feature module export
an Axum router plus a plugin-trait implementation that hooks into core
communication.

**Agree.** A `Plugin` trait with per-plugin routes is the right shape.
Capabilities and packet dispatch then follow from the list of plugins, not
from hand-kept tables.

**Further: routing is the easy part.** Most of the coupling isn't in
dispatch. The hard parts are what the trait has to answer:

1. **State and locking.** One `RwLock<ApplicationState>` holds every
   feature's state. Each plugin should own its state behind its own lock,
   with one rule: *core never calls into a plugin while holding its own
   lock*, and plugins never hold theirs while calling core.
2. **Events.** `EventData` is a closed enum that every feature edits.
   Core keeps typed variants for core resources, and plugins publish
   through one open `Plugin` variant with a typed encode/decode helper (§5.3).
3. **Errors.** `ApplicationError` and `map_error` are also closed. Core
   errors (`NotPaired`, `DeviceNotConnected`, `UnsupportedByPeer`, …) stay
   in core. A plugin's own errors map to the shared `ApiProblem` inside the
   plugin.
4. **Plugins that add to core resources.** Battery is a field on
   `DeviceSnapshot`, and `clipboardSyncEnabled` is a field in settings. A
   plugin needs a sanctioned way to add a field to a device and to own a
   settings section (§5.5, §5.6). Otherwise these leak back into core.
5. **Lifecycle.** Browsing sessions and transfers are torn down from four
   places in core, and clipboard sends a packet on connect. The trait needs
   `connected`, `disconnected`, `unpaired` and `shutdown` hooks.
6. **Shared services.** Share and browsing both create transfers. The
   transfer registry (state machine, progress throttling, tasks,
   `/transfers`) becomes a core service that both use, not something one
   plugin exports to another.

**Push back:**

- **Pairing stays in core,** as agreed. It gates every other packet and
  owns trust, so making it a plugin would mean plugins that can veto
  plugins.
- **No distributed registration** (`inventory`, `linkme`, build scripts).
  One line per plugin in a `builtin()` list is a trivial conflict and is
  easy to read. Link-time magic costs more than it saves.
- **No plugin-to-plugin dependencies.** If two features need the same
  thing, it goes into core (as transfers do). Otherwise the dependency
  graph between plugins becomes the new tangle.
- **Remove the `ApplicationService` trait, `Query`/`Command`/`QueryResult`
  and the forwarding impl** rather than modularizing them. They are central
  lists with one implementation. Core routes can take the core handle
  directly.
- **Don't expect zero central edits.** What remains is one `builtin()` line
  per plugin and, for now, `client.rs`, the CLI, the UI and ARCHITECTURE.md.
  The measure is that `service.rs`, `api.rs`, `events.rs`, `state.rs` and
  `ApplicationError` stop changing for feature work.

## 4. Options considered

| Option | Verdict |
| --- | --- |
| A. Keep the fixed table, split `service.rs` into per-feature `impl ApplicationHandle` files (as `service/browse.rs` does) | Cheapest, and it shrinks the file. But all shared lists (state, events, errors, routes, trait, dispatch) stay central, so conflicts move rather than go away. Rejected as the end state; it's roughly what phase 5 does for core-only code. |
| B. **`Plugin` trait with per-plugin state, routes and hooks, and a small core** | **Recommended.** Removes every central list except the registry line. |
| C. B plus a crate per feature | Declined by the owner. Can be done later without redesign, since B already makes plugins depend only on the core API. |
| D. Message-passing actors per plugin (each plugin a task with an inbox) | More isolation, but every call becomes async request/response and the current synchronous handlers would need rewriting. Unnecessary: per-plugin locks give the same isolation here. |

## 5. Target shape

This was the plan. §8 records where the result differs; `ARCHITECTURE.md`
§2 describes it as built.

### 5.1 Layout

```text
src/
  protocol/ transport/ config/     unchanged
  application/                     the core (rename to core/ in phase 5)
    plugin.rs                      Plugin trait, PluginContext, PluginRegistry, PluginEvent
    devices.rs connections.rs      registry and live connections (phase 5, out of service.rs)
    pairing.rs transfers.rs        core state machines (phase 5)
    events.rs settings.rs          core events and settings, open to plugins
    http.rs                        core routes (/status, /devices, /pairings, /transfers,
                                   /settings, /events) and ApiProblem
  plugins/
    mod.rs                         builtin(): the one list of plugins
    ping/     mod.rs  packet.rs  http.rs
    battery/  mod.rs  packet.rs
    clipboard/ mod.rs packet.rs  http.rs   (+ the clipboard backends, now crate::clipboard)
    share/    mod.rs  packet.rs  http.rs
    browse/   mod.rs  packet.rs  http.rs  session.rs  ssh.rs  files.rs
              (was sftp.rs + service/browse.rs + transport/sftp.rs + application/files.rs)
  api.rs                           server shell: listener, auth, limits, SSE, merging routers
```

Dependencies point one way: `plugins/* → application (core API) →
protocol, transport, config`. Plugins never import each other. The core
reaches plugins only through `dyn Plugin`, except for the single
`plugins::builtin()` call at the composition root.

### 5.2 The trait

```rust
pub trait Plugin: Send + Sync + 'static {
    /// Stable identifier, e.g. "ping"; for logs and settings sections.
    fn id(&self) -> &'static str;
    /// Packet types this plugin receives and sends; the identity packet's
    /// capabilities are the union over all plugins.
    fn incoming(&self) -> &'static [&'static str];
    fn outgoing(&self) -> &'static [&'static str];

    /// A packet of one of `incoming()`'s types from a paired device.
    /// Pairing is checked by core before this is called.
    fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet);

    /// HTTP routes, already given their state. Merged under /api/v1 with
    /// the standard body limit and request deadline.
    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router { Router::new() }
    /// Streaming routes: own body limit, no overall deadline (uploads).
    fn streaming_routes(self: Arc<Self>, ctx: PluginContext) -> Router { Router::new() }

    // Lifecycle, all optional (phases 1–4 add them as plugins need them):
    fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) {}
    fn disconnected(&self, ctx: &PluginContext, device_id: &str) {}
    fn unpaired(&self, ctx: &PluginContext, device_id: &str) {}
    fn device_state(&self, device_id: &str) -> Option<Value> { None }  // keyed by id()
    fn started(self: Arc<Self>, ctx: &PluginContext) {}  // added in phase 4
    fn shutdown(&self) -> BoxFuture<'_, ()> { Box::pin(async {}) }
}
```

Handlers stay synchronous and spawn tasks when they need to, as they do
today, so the trait is dyn-compatible without `async_trait`. Core passes
the context into each call instead of plugins storing it. That way there is
no `Arc` cycle between the core handle and its plugins.

### 5.3 What core exposes: `PluginContext`

A cloneable handle over the core. It exposes only what features need:

- **Devices:** `device(id)`, and `device_changed(id)`, which republishes
  `device.updated` after a plugin's `device_state` changes.
- **Sending:** `send(device_id, packet)` checks that the device is paired
  and connected and that the peer advertised the packet type, then queues
  the packet (today's `capable_connection`). Also `broadcast(packet, except)`.
- **Events:** `publish(&T)` for any `T: PluginEventKind` (a
  `Serialize + DeserializeOwned` type with `const TYPE: &str`). On the bus
  that becomes `EventData::Plugin { type, data }`. The wire format is
  unchanged (`{"type": "ping.received", "data": {…}}`), and Rust
  consumers call `event.decode::<ReceivedPing>()`.
- **Transfers** (phase 3): `transfers().begin(device, direction, name,
  size)` returns a handle with `progress`, `complete`, `fail` and a
  cancellation token. The handle owns the state machine and the 100 ms
  progress throttle, and is cleaned up on drop.
- **Payload connections and TLS material** (phase 3): what share and
  browsing use to dial or accept pinned connections, without handing out
  the private key directly.
- **Settings** (phase 2): `settings::<T>(id)` and a watch of the plugin's
  section (§5.6).

### 5.4 HTTP

`api.rs` becomes the server shell. It merges the core routes and
`routes()` of every plugin, then applies the deadline and auth layers.
Separately it merges `streaming_routes()` with the large body limit.
This replaces `enforce_content_length`'s path-suffix check
(`/transfers`, `/files/upload`), which is the one place where a plugin's
URL is hard-coded into the shell. Overlapping routes panic when the
router is built, so a clash between plugins fails every test at once.

`ApiProblem` and `impl From<CoreError> for ApiProblem` become crate-public,
so plugin handlers can `?` core errors and build their own problem codes.

Paths are resources under the device they act on
(`/devices/{id}/ping`, `/devices/{id}/files/...`). Global plugin state has
its own top-level path (`/clipboard`). No `/plugins/<id>/` prefix: it
would change every URL to express something clients don't care about.

### 5.5 Adding to the device snapshot

`DeviceSnapshot` stops having a `battery` field. Core builds a
`plugins: {"battery": {...}}` map from each plugin's `device_state`,
keyed by plugin id; a plugin with nothing to add has no key. A plugin that
changes its state calls `ctx.device_changed(id)`. The UI keeps its
one-snapshot model (ADR 0003): still one list endpoint and one
`device.updated` event. Clearing on disconnect or unpair is the plugin's
job, via its hooks.

Decided in phase 1: a nested `plugins` map, not flattened into the
snapshot. Flattening would have kept `battery` where it was, but it puts
plugin keys in the same namespace as core fields, so a plugin id could
shadow a future core field, and a client deserializing with a catch-all
(`#[serde(flatten)]` into a map) can't tell an unknown core field from
plugin state. The nested map keeps the two apart, lets a client ignore
plugins it doesn't know as a whole, and is always present (`{}` when
empty), so clients need no null handling for it.

### 5.6 Settings sections

Core settings stay typed (`deviceName`, `downloadDir`, `closeToTray`).
Each plugin that has settings owns a typed section: it is stored under
its id in `settings.json`, has defaults and validation in the plugin,
and is exposed in `GET/PATCH /settings` under its id. A change still
publishes one `settings.changed`. `clipboardSyncEnabled` moves to
`clipboard.syncEnabled`. This is the one planned wire change, and the UI
and CLI change with it in phase 2.

Decided in phase 2: sections sit under a `plugins` object, as in device
snapshots (§5.5), so the setting is `plugins.clipboard.syncEnabled`, in
both `settings.json` and `/settings`. See §8.

### 5.7 Testing

- A plugin is unit-tested against a real core built by a small test kit
  (`application::testing`). The kit has an in-memory trust store, a fake
  paired connection whose outgoing packets are an `mpsc::Receiver`, and an
  event subscription. That is the `handle()` helper in `service.rs`'s
  tests, made shared.
- The e2e tests (`tests/*_e2e.rs`) stay as they are. They already test
  per feature.

## 6. Migration

Each phase is one PR that leaves every "done means" check green. The order
takes one new trait feature at a time.

| Phase | Moves | Proves |
| --- | --- | --- |
| 0 (done, #16) | ping | trait, registry, dispatch, capabilities, routes, open plugin events, `ApiProblem` for plugins |
| 0b (done) | find my phone (ring), added after the plan the old way | a send-only plugin: `incoming()` and `handle_packet()` got defaults |
| 1 (done) | battery | `device_state` + `device_changed`, `disconnected`/`unpaired` hooks, `DeviceSnapshot` extension (UI change) |
| 2 (done) | clipboard | settings sections (UI + CLI change), `connected` hook, `broadcast`, plugin-owned global resource (`/clipboard`) |
| 3 (done) | share | transfers extracted into a core service; `streaming_routes`; payload/TLS access through the context |
| 4 (done) | browse (sftp) | plugin-owned sessions, `shutdown` (and `started`) hooks; `service/browse.rs` and `transport/sftp.rs` move into the plugin; SSH sign-in through the context; the clipboard follower moves into its plugin |
| 5 (done) | core cleanup | split what is left of `service.rs` into `devices`/`connections`/`pairing`; remove `ApplicationService`, `Query`, `Command`/`QueryResult` (the LAN command channel stays, as a core-internal type); rename `application` → `core`; move `RunningService` to a composition-root module; update ARCHITECTURE §2 and remove the "deliberately not a plugin system" notes |

Phases 1–4 are independent enough to run in parallel worktrees once
phase 0 lands. That is the goal in miniature.

## 7. Pilot: ping (phase 0)

Implemented on this branch:

- `application/plugin.rs`: `Plugin`, `PluginContext`, `PluginRegistry`,
  `PluginEvent`/`PluginEventKind`.
- `plugins/ping/`: `packet.rs` (the old `ping.rs`), `mod.rs` (the plugin,
  `send_ping`, `ReceivedPing`), `http.rs` (`POST /devices/{id}/ping`).
- Removed from core: the ping arm in `handle_peer_packet`,
  `send_ping` from `ApplicationHandle` and `ApplicationService`,
  `EventData::PingReceived`, `ReceivedPing` from `state.rs`, `post_ping`
  from `api.rs`.
- The other features still go through the fixed table. `plugins::
  capabilities()` merges both, so the identity packet doesn't change.
- Wire contract unchanged: same route, same `ping.received` JSON, same
  error codes. The UI needs no change.

Findings from the pilot are recorded in §8.

## 8. Findings

All "done means" Rust checks pass (fmt, clippy `-D warnings`, 185 tests).
A daemon started with a token was probed live: the plugin's route returns
`401` without the token and `404 device_not_found` with it, so the route
sits behind auth and uses the shared problem format.

- **Ping now lives entirely in `plugins/ping/`,** including its unit
  tests. `service.rs` lost 150 lines net. The core files this touched
  (`api.rs`, `events.rs`, `state.rs`, `service.rs`) only lost ping code or
  gained code for all plugins; a second plugin won't touch them for the
  same reasons.
- **One `impl Plugin` per feature.** Routes are a trait method, so a
  plugin declares packet types, handler and routes in one place. Its
  `http.rs` is only where the handlers live.
- **Open events work without a wire change.** `#[serde(untagged)]` on the
  last `EventData` variant serializes plugin events exactly like core
  ones, and anything the core doesn't recognise deserializes as
  `EventData::Plugin`. Caveat: a *malformed* core event now also parses,
  as a `Plugin` event, instead of failing. Clients were already skipping
  events they couldn't use, so this seems acceptable. A custom
  `Deserialize` could close the gap if it matters.
- **Axum state composes cleanly.** Core routes call `.with_state()`
  before merging, so plugin routers (`Router<()>`, with their own state)
  merge in and share the deadline, body limit, auth and fallback layers.
- **The test kit was needed at once.** Ping's tests had used private
  helpers in `service.rs`, so `application::testing` (in-memory trust
  store, `handle()`, `make_identity`) was extracted in this phase rather
  than later. Every later plugin gets it for free.
- **Temporary seams to remove in phase 5:**
  - `ApplicationHandle::new` calls `plugins::builtin()`, an edge from
    core to features. It belongs at the composition root.
  - `plugins::capabilities()` builds a second registry just to read
    capability lists. It should come from the handle's registry.
  - `ApplicationService` gained `plugin_routes()`. That is one more sign
    the trait should go.
- **Rust callers change shape.** `handle.send_ping(..)` became
  `ping::send_ping(&handle.plugin_context(), ..)`. That's fine for tests
  and the CLI (which uses HTTP anyway), and it keeps feature functions out
  of the core's type.
- **Capability filtering is only tested through ping and ring** (as
  before). When battery or clipboard lands, move one of those tests to
  `plugin.rs` against a dummy plugin, so the core's `send` check has its
  own test.

Moving find my phone (#15), which landed the old way while the plan was
in review, showed:

- **It confirmed the problem.** #15 edited `service.rs`, `api.rs`, the
  `ApplicationService` trait and the capability table, and it conflicted
  with #16 in three of them. As a plugin it is two small files, and
  `service.rs` and `api.rs` only lost lines.
- **A send-only plugin needed defaults.** Ring handles no packets, so
  `incoming()` now defaults to none and `handle_packet()` to a no-op.
  A plugin that declares incoming types still has to override the
  handler; its tests would catch a missing one.
- **Capability order moved.** Plugins' capabilities come first, so ring's
  string moved in the identity packet. Order means nothing to peers, and
  the capabilities test now compares sorted lists so the next move won't
  break it.

Moving battery (phase 1) showed:

- **Pull, not push, for device state.** The plugin keeps each device's
  last report behind its own lock and answers `device_state(id)`; the
  core asks every plugin whenever a snapshot leaves it (the five device
  events, `GET /devices`, `GET /devices/{id}`), always after dropping its
  state lock. The registry in `device.rs` never holds plugin state, so it
  lost `set_battery` and the clearing in `mark_disconnected`/`set_paired`.
  A push model (`ctx.set_device_state`) would have put plugin data back
  into core state and made clearing core's job again.
- **`device_state` returns `Option<Value>`, keyed by `id()`,** instead of
  the planned `(&'static str, Value)`. A second key per plugin had no use
  and allowed two plugins to collide; the registry now also panics on a
  duplicate id, like it does on a duplicate packet type.
- **Hooks run before the core publishes.** `disconnected` runs before
  `device.disconnected` and `unpaired` before the `device.updated` (peer
  unpaired us) or `device.forgotten` (we forgot it). So the event already
  shows the battery gone, and a plugin clearing state in a hook needn't
  call `device_changed`. Forgetting a connected device calls both hooks.
  Without this ordering every disconnect would have published an extra
  `device.updated`.
- **Wire change, as §5.5 records.** `battery` moved to
  `plugins.battery`. The CLI and `client.rs` needed no code of their own:
  they share `DeviceSnapshot`, and read the battery through
  `BatteryStatus::of(&device)` from the plugin. The UI keeps a `battery`
  getter on `Device` that decodes `plugins['battery']`, and ignores a
  shape it doesn't know rather than failing to decode the whole device, so
  widgets didn't change.
- **Core files only lost battery code or gained shared code.** `service.rs`
  lost 44 lines net (the dispatch arm, `handle_battery`, its test) and
  gained `with_plugin_state` and the hook calls. `api.rs`, `events.rs`,
  `state.rs` and `ApplicationError` didn't change. `plugins/mod.rs` lost
  the battery arm of the fixed table and gained one `builtin()` line.
- **The first stateful plugin needs construction.** `builtin()` now holds
  `BatteryPlugin::default()`. `plugins::capabilities()` still builds a
  throwaway registry to read capability lists (a phase 5 seam), which now
  also allocates the plugin's empty map; harmless, but one more reason to
  read capabilities from the handle's registry.
- **The core's `send` check has its own test.** As §8 asked, the
  capability-filtering test moved from ping to `plugin.rs`, against a
  dummy send-only plugin: unknown device, not paired, not connected, not
  advertised, then accepted. Ping keeps a test that it sends its body.
- **Still to verify on a real phone,** as before the move: the checks
  here ran against the fake phone (unit tests, `browse_e2e`, and the app
  showing `Connected · 73%` and clearing it on disconnect).

Moving clipboard (phase 2) showed:

- **Settings sections are nested under `plugins`,** not top-level ids as
  §5.6 first sketched: `{"deviceName": …, "closeToTray": …, "plugins":
  {"clipboard": {"syncEnabled": true}}}`. The reasons from §5.5 hold
  (a plugin id can't shadow a future core setting, a client can ignore
  unknown plugins as a whole), clients already know the shape from device
  snapshots, and the core patch keeps `deny_unknown_fields`, which serde
  can't combine with a flattened map. The UI mirrors it as it does for
  devices: `DaemonSettings.plugins` plus a `clipboardSyncEnabled` getter,
  so the settings page didn't change.
- **Typed in the plugin, opaque in the core.** A plugin declares a
  `PluginSettings` type (serde defaults, `deny_unknown_fields`, its `ID`)
  and returns `SettingsSection::of::<T>()` from `Plugin::settings`. The
  core stores only the fields the user set, merges a patch into them
  (`null` resets a field, a `null` section resets the section), accepts
  the result only if it deserializes as `T`, and reports `400
  invalid_settings` otherwise, or for an unknown section. `GET /settings`
  lists every section with defaults filled in. No separate validation hook
  was needed; one can be added when a plugin has a rule its type can't
  express.
- **Pull, not watch, for settings too.** The plugin reads its section when
  it acts (`ctx.settings::<ClipboardSettings>()`), before taking its own
  lock. §5.3's watch wasn't needed: nothing in clipboard has to react to a
  change as it happens. A plugin that must (e.g. to stop a server) can get
  a `settings_changed` hook then.
- **No migration, by the owner's call.** The project is pre-release, so
  a stored `clipboardSyncEnabled` is ignored like any unknown key (sync
  falls back to its default, on) and dropped on the next save. A migration
  was written and then removed in review; if a later plugin needs one, it
  would need the file's unknown top-level keys kept in memory, which
  `StoredSettings` doesn't do.
- **`connected` runs after `device.connected` is published,** with the
  device's snapshot, never under the core's lock. Clipboard offers its text
  there as `kdeconnect.clipboard.connect` through `ctx.send`, so the
  capability checked is now the connect packet's own type rather than
  `kdeconnect.clipboard` (KDE Connect advertises both). The device may not
  be paired yet; `send` refuses it then, as the old code did.
- **`broadcast(packet, except)` lives in the core** and has its own test
  in `plugin.rs`. Clipboard uses it both for local changes and to forward
  text from one peer to the others. Also added: `ctx.can_send(device,
  type)`, so "send clipboard" reports `device_not_found` or
  `unsupported_by_peer` before `clipboard_empty`, as before, although it
  reads the clipboard before building the packet.
- **A plugin with a dependency moved `builtin()` out of the core.**
  Clipboard needs its backend (desktop or in-memory), chosen at start. So
  `ApplicationHandle::new` now takes the plugin list, and
  `plugins::builtin(clipboard)` is called by `RunningService` and the tests.
  That removes the phase 0 seam "`ApplicationHandle::new` calls
  `plugins::builtin()`". `plugins::capabilities()` still builds a throwaway
  registry, now with an in-memory clipboard (phase 5).
- **The composition root needs its plugin back.** Following the desktop
  clipboard is a task started with the daemon, and there is no start hook,
  so `RunningService` fetches the plugin with `ApplicationHandle::
  plugin::<ClipboardPlugin>()` (a downcast; `Plugin` now has `Any` as a
  supertrait) and stops the backend on shutdown, as it did before. The
  lookup is on the handle, not on `PluginContext`, so plugins still can't
  reach each other. Phase 4's `shutdown` hook, and a matching start hook,
  could move this into the plugin.
- **Core files only lost clipboard code or gained shared code.**
  `service.rs` lost 506 lines net (the state, the handlers, the
  `ApplicationService` methods, ten tests, which moved to the plugin),
  `api.rs` lost 54 (three routes and two error mappings), `state.rs` lost
  `ClipboardSnapshot` and the clipboard query, `events.rs` lost
  `ClipboardChanged`, and `ApplicationError` lost two clipboard variants
  and gained the shared `InvalidSettings`. `settings.rs` gained sections.
  The backends moved from `crate::clipboard` into `plugins/clipboard/
  backend.rs` and `backend/system.rs`.
- **Wire changes:** only the settings one. `/clipboard`,
  `/devices/{id}/clipboard`, `clipboard.changed` (now published as a plugin
  event, same JSON) and the error codes are unchanged. A `PATCH` still
  sending `clipboardSyncEnabled` gets `422`, like any unknown field. Rust
  callers change shape: `handle.set_clipboard(text)` became
  `handle.plugin::<ClipboardPlugin>()?.set_text(&ctx, text)`, and the
  client and CLI decode `clipboard.changed` with
  `PluginEvent::decode::<ClipboardSnapshot>()`.
- **Checked live** in the app under Xvfb against a CLI peer: text set on
  the peer reached the app's X clipboard, a copy on the app's display
  reached the peer, a restarted peer got the app's text from the
  connect-time offer, and with the switch off neither direction synced.

Moving share (phase 3) showed:

- **Transfers are a core service with a handle, not a plugin export.**
  `application/transfers.rs` holds every transfer behind its own lock, not
  the core's state lock. `ctx.transfers().begin(device, direction, name,
  size)` records a `queued` transfer, publishes `transfer.started`, and
  returns a `TransferHandle`: `connecting()`, `transferring()`,
  `progress(n)` (the 100 ms throttle moved here from `service.rs`), and
  `complete`, `fail`, `cancelled` or `finish(result)` to end it. Ending
  consumes the handle. `handle.spawn(|handle| async { .. })` runs the
  transfer on a task the core can wait for at shutdown. The core keeps
  `GET /transfers`, `DELETE /transfers/{id}` and cancelling a
  disconnected device's transfers, whatever started them.
- **Cleanup on drop.** A handle dropped without ending its transfer fails
  it with `internal`, or marks it `cancelled` if cancellation was asked
  for, so a transfer whose task panics or is aborted at shutdown never
  stays `transferring`. Ending one in a state it can't reach from where it
  is (completing a transfer that never started moving bytes) fails it
  rather than leaving it running. Before this, each feature had to call
  `cleanup_transfer_task` on every exit path, and browse tracked its task
  after spawning it, so it had to check whether the task had already
  finished.
- **Shared copy loops live on the handle.** `handle.copy(reader, writer)`
  and `handle.forward(chunks, writer)` wrap `transport::payload`'s bounded
  loops with the handle's cancellation and progress.
  `handle.save_to_downloads(reader, name)` is the whole "partial file,
  rename into place, unique name, remove on failure" sequence, which share
  and browse had each written out. `upload_channel()` is the one bounded
  channel an HTTP upload feeds. Browse (still in core until phase 4) now
  uses all of these, and its download, upload and cancellation still pass
  `browse_e2e` and a live check against the fake phone. It touches its
  session once a copy ends instead of on every chunk; the session's
  `Arc` already keeps it open while a copy runs.
- **Payload connections without the key.** `ctx.payload_peer(device)`
  checks that the device is paired and connected, and captures its pinned
  certificate, its address, and the payload settings. `peer.listen()`
  returns a `PayloadListener` (`port()`, then `accept()`), and
  `peer.connect(port)` dials. The TLS material is built inside
  `application/payload.rs`, so plugins get authenticated streams and
  never the key. Browsing still reads the key directly for its SSH
  session; that goes behind the context in phase 4.
- **`streaming_routes` replaced the path sniffing.** The server now builds
  two routers. Normal routes get the deadline and the 64 KiB
  `Content-Length` check. Streaming routes (every plugin's
  `streaming_routes()`, plus browse's upload until phase 4) get the
  transfer-sized `DefaultBodyLimit` and `Content-Length` check, no
  deadline, and an `UploadIdleTimeout` request extension. Axum applies a
  router's layers to the routes it has when they are added, so the two
  sets keep their own limits after the merge. The multipart helpers
  (`next_field`, `declared_size`, `forward_upload`, `skip_field`) moved to
  `api/upload.rs` for plugins to share. A route registered through
  `routes()` by mistake fails with a missing extension rather than
  silently getting the small limit. `ApplicationService` gained
  `plugin_streaming_routes()`, which phase 5 will remove with the trait.
- **Wire change: sending a file is `POST /devices/{id}/share`.** It was
  `POST /transfers` with a `deviceId` part that had to come before the
  file. `/transfers` is a core resource, and a plugin adding `POST` to a
  core path would have tied the core's URL space to one plugin. §5.4 puts
  device actions under the device, like `/ping`, `/ring` and
  `/clipboard`. The body is now just the `file` part.
  `POST /transfers` answers `405`; there is no alias, since the project is
  pre-release. The UI's `sendFile`, `client.rs` and so the CLI changed
  with it. Transfer snapshots, the `transfer.*` events and every error code
  are unchanged.
- **The identity packet didn't change.** Share claims only
  `kdeconnect.share.request`, in both directions. The old fixed table
  parsed `kdeconnect.share.request.update` and ignored it. Now nothing
  claims it, so the core drops it the same way, and it isn't advertised
  (it never was).
- **Core files only lost share code or gained shared code.** `service.rs`
  lost 663 lines: the share handlers, both payload loops, the transfer map
  and task bookkeeping, `ProgressEvents`, and the dead
  `Command::StartTransfer`/`CancelTransfer`. It gained `device()`,
  `transfers()` and `payload_peer()` for the context. `api.rs` lost 150
  lines: `POST /transfers`, the path constants and the upload helpers, now
  in `api/upload.rs`. `browse.rs` lost 208. `events.rs` and
  `ApplicationError` didn't change: the transfer events and errors were
  already shared by share and browse. `plugins/mod.rs` lost the share
  arms of the fixed table and gained one `builtin()` line.
- **Checked live**, isolated (temporary data and download dirs, loopback
  discovery, a private Xvfb display and D-Bus session):
  - Between two CLI daemons: files both ways, a 1 GiB upload through the
    streaming route (156 `transfer.progress` events in 15.6 s, byte
    identical), and cancelling mid-flight from either end, which left no
    partial file.
  - In the app against a CLI peer: receiving a file, cancelling a large
    incoming one, and sending 1 GiB through the file picker, with
    progress, then cancelling a second send.
  - Against the fake phone: browse download, upload and cancel, and a
    download stopped by the phone disconnecting.
- **Pre-existing problems this surfaced, not fixed here:**
  - `ApiClient::watch_transfer` reads the snapshot before subscribing to
    `/events`, so a transfer that ends in between leaves
    `ferry send --watch` waiting.
  - When an upload's transfer is cancelled, the handler still reads the
    rest of the multipart body to look for further parts. A large upload
    then ends in `408` after the idle timeout rather than at once. The
    old `POST /transfers` did the same.
  - Both were fixed afterwards: the client subscribes before loading a
    snapshot, and an upload whose transfer ends is answered at once.

Moving browse (phase 4) showed:

- **The fixed table is gone.** Browse was its last user, so
  `plugins::dispatch_incoming`, `IncomingPluginPacket`,
  `PluginDispatchError` and `legacy_capabilities` went with it, and
  `handle_peer_packet` drops any packet type no plugin claims. The
  identity packet didn't change: the plugin claims `kdeconnect.sftp`
  incoming and `kdeconnect.sftp.request` outgoing, as the table did.
- **Sessions are the plugin's state.** `BrowsePlugin` owns `Sessions`
  (`session.rs`): the per-device slots whose async lock serializes
  opening, the offers being waited for, and the idle watchers' token,
  behind its own locks. It closes a device's session in `disconnected`
  and `unpaired`, which the core already calls from the four places that
  used to call `close_browse_session` (unregistering a connection,
  forgetting, and the peer unpairing us). Checking the device moved to
  `ctx.can_send(device, "kdeconnect.sftp.request")` plus
  `ctx.payload_peer(device)`, so `browse_connection` and `known_device`
  went; the errors and their order (unknown, not paired, unsupported,
  not connected) are the same. The operations are methods on the plugin
  taking the context, like clipboard's: Rust callers write
  `handle.plugin::<BrowsePlugin>()?.upload(&ctx, ..)`.
- **`shutdown` is `fn shutdown(&self) -> BoxFuture<'_, ()>`,** run for
  every plugin concurrently (`join_all`) by `ApplicationHandle::
  shutdown_plugins`, which `RunningService` calls after
  `shutdown_transfers`, as `shutdown_browsing` was. The order matters:
  a transfer holds its session, and a session is only closed politely
  once nothing holds it. A new `browse_e2e` test opens a session and
  checks that `shutdown_plugins` closes its SSH connection.
- **A `started` hook came with it, for clipboard.** Phase 2 left the
  desktop clipboard's follower, and releasing the clipboard at shutdown,
  in `RunningService`, reached through `plugin::<ClipboardPlugin>()`.
  `Plugin::started(self: Arc<Self>, ctx)` is called once by
  `RunningService` (`ApplicationHandle::start_plugins`) inside the
  runtime, before the transport starts. It isn't called from
  `ApplicationHandle::new`, because unit tests build cores outside a
  runtime, where spawning would panic, and most don't want background
  work. It takes `Arc<Self>` because the work it starts outlives the
  call. `ClipboardService` gained two defaulted methods,
  `watch_local_changes()` and `release()`, so the plugin follows and
  releases whatever backend it was given without knowing it is the
  desktop's. `RunningService` lost its clipboard field and the downcast;
  `ApplicationHandle::plugin` is now used only by tests. The clipboard
  is now released after transfers end rather than before; nothing
  depends on that order.
- **The SSH key goes through the core.**
  `PayloadPeer::authenticate_ssh(&mut ssh, user)` signs in to an SSH
  server on the device with this device's key and returns whether the
  server took it. The plugin connects, checks the host key against
  `peer.certificate_der()`, dials `peer.ip()`, and falls back to the
  offer's password itself. Two alternatives were weighed: implementing
  russh's agent-style `Signer` in the core would have kept the core
  unaware of SSH sessions but meant producing SSH signature blobs by
  hand; handing out an opaque key wrapper would still have given the
  plugin the key to pass to russh. The cost of the chosen one is that the
  core's API names russh types (`client::Handle`, `client::Handler`); the
  core already depended on russh.
- **Errors moved with the feature.** The ten browse variants of
  `ApplicationError` (`InvalidRemotePath`, `RemoteFile*`,
  `NotADirectory`, `IsADirectory`, `RemoteHostKeyMismatch`,
  `RemoteFiles*`) became `BrowseError` in the plugin, with a `Core`
  variant for core errors, and their `map_error` arms became its
  `From<BrowseError> for ApiProblem`, with the same status codes and
  problem codes. `ApiProblem` gained `with_detail` for
  `files_unavailable`'s detail. `InvalidFileName` and `TransferTooLarge`
  stay in the core: share uses them too.
- **Types moved; the wire didn't.** `DirectoryListing`, `FileEntry` and
  `FileKind` are now `plugins::browse::*` (the client and CLI import
  them from there), and the upload route is the plugin's
  `streaming_routes()`, so the server's streaming router holds only
  plugin routes. Routes, JSON, events and error codes are unchanged; the
  plugin's id, `browse`, appears nowhere on the wire, since it adds no
  device state or settings.
- **Core files only lost browse code or gained shared code.**
  `service/browse.rs` (805 lines) left the core, `api.rs` lost 238 lines
  net, `service.rs` 133, `plugins/mod.rs` 71 (the fixed table and its
  tests), and `application.rs` 25 (the clipboard wiring). The core gained
  the two hooks and their registry calls, `start_plugins`/
  `shutdown_plugins`, `PayloadPeer::{ip, certificate_der,
  authenticate_ssh}`, `ApiProblem::with_detail`, and
  `testing::handle_with_plugins`.
- **Also fixed here: `--discovery-loopback` was reachable from the LAN.**
  It only changed where announcements went: discovery stayed bound to
  `0.0.0.0:1716`, and the control listener and payload ports to
  `0.0.0.0`, so during phase 3's checks a real phone on the LAN
  connected to a daemon started that way. `LanConfig::loopback(port)` now binds
  discovery to `127.255.255.255` (a socket on `127.0.0.1` doesn't
  receive loopback broadcasts, so loopback instances couldn't find each
  other) and the control listener to `127.0.0.1`; `RunningService` also
  binds payload ports to `127.0.0.1` in that mode. A device added by a
  loopback address is reached by the loopback broadcast; one off
  loopback isn't announced to. `ss -lunpt` for a daemon, and for the app,
  now shows only `127.255.255.255:1716` and `127.0.0.1` listeners (it
  showed `0.0.0.0:1716` for UDP and TCP before). A test in `tests/lan.rs`
  checks the binds and that loopback instances still meet, by broadcast
  and by address. The fake phone example listens on the loopback
  broadcast too.
- **Checked live**, isolated (temporary dirs, loopback discovery, free
  ports, a private Xvfb display and D-Bus session):
  - CLI daemon against the fake phone: list, preview (`cat`), download,
    upload, mkdir, move, recursive delete and the error codes
    (`file_not_found`, `invalid_path`, `not_a_directory`); cancelling a
    1.5 GB download and a 1 GiB upload mid-flight (no partial file on
    either side); the phone disconnecting mid-download (transfer ends,
    partial removed); and `SIGINT` with an open session and a running
    download (the daemon exits, the SSH connection closes, no partial).
    Key sign-in through the core worked; a restarted fake phone, which
    forgets the desktop's key, fell back to the password as intended.
  - The app against the fake phone: storage roots, a folder, downloading
    a small file and 1.5 GB (byte-identical), uploading through the file
    picker, cancelling a 2 GiB upload from the Transfers page (removed
    from the phone), the phone disconnecting mid-download ("Connect Fake
    Phone to browse its files", partial removed), and Quit from the tray
    menu with a session open (the app exited in about 0.2 s and the SSH
    connection closed).
  - Clipboard after the move: three CLI daemons, two sharing an Xvfb
    display with `--system-clipboard`; text set through one reached the
    display's clipboard, the other followed it as a local copy and synced
    it to its paired peer, and each daemon stopped within 0.2 s.

The core cleanup (phase 5) showed:

- **`application` is `core`, and its types followed.** `ApplicationHandle`
  is `Core`, `ApplicationError` is `CoreError`, `ApplicationEvent` is
  `CoreEvent`. A module named `core` at the crate root shadows the `core`
  crate only for `use core::…` written in `lib.rs` itself; nothing does,
  and macros use `::core`. `crate::device` moved in as `core/devices.rs`
  (`ferry::device::*` is now `ferry::core::*`), since the registry
  is core state that nothing else touches.
- **The trait, `Query`, `QueryResult` and `Command` are gone.** The API
  server takes the `Core` and calls plain methods (`status()`,
  `devices()`, `device(id)`, `pairings()`, `pairing(id)`,
  `transfers().list()`, `settings()`). With them went the handlers'
  "unexpected query result → 500" arms and `UnexpectedQueryResult`, and
  `GET /status` can no longer fail. `Command` is `LanCommand`, the
  channel from the core to the LAN transport, with the two variants it
  actually carried (`AnnounceDiscovery`, `AnnounceTo`); the four pairing
  and forget variants were never sent. `announce()` replaces
  `command(Command::AnnounceDiscovery)`. The error codes
  (`command_queue_full`, `application_unavailable`) didn't change.
- **Plugin routes are a crate-internal method,** `Core::plugin_routes()`
  and `plugin_streaming_routes()`, called only by the server. They could
  have moved to the composition root (build the routers there and hand
  them to `ApiServer::start`), but that would make every test that starts
  an API server build them too, for no gain.
- **The split is by file, not by lock.** `service.rs` became
  `core/devices.rs`, `core/connections.rs` and `core/pairing.rs` (plus
  `core/error.rs`), each an `impl Core` block for what it acts on, over
  the same `CoreState` behind the same lock. Devices, connections and
  pairings change together (accepting a pairing reads the connection,
  writes trust and flips the device's `paired` and `pairing` at once),
  so separate locks would have added ordering rules for nothing. `Core`
  and `CoreState` sit in `core.rs`, the module root, so the child modules
  see their private fields without widening them; methods one child calls
  in another are `pub(super)`. `state.rs` dissolved: the pairing types went
  to `pairing.rs`, the transfer types to `transfers.rs`. The moves were
  split into their own commits so `git log --follow` and
  `--color-moved` show them as moves. `service.rs` was 3,192 lines when
  this plan started and 1,683 at the start of this phase; the largest core
  file is now `transfers.rs` (1,049 lines, 280 of them tests).
- **The composition root is `daemon.rs`.** `RunningService`, `RunRequest`
  and `run_service` moved there, and it is the only non-test caller of
  `plugins::builtin()`. The core now names no plugin, and `plugins` is
  reached from the core only through `dyn Plugin`.
- **Capabilities come from the running core.** `Core::capabilities()`
  (from `PluginRegistry::capabilities()`) replaced `plugins::
  capabilities()` and its throwaway registry; the daemon and the e2e tests
  advertise exactly what the core they built runs. The identity packet
  didn't change; its test now reads the registry of `builtin()`.
- **`Core::plugin::<T>()` is gone,** with `PluginRegistry::get` and the
  `Any` supertrait on `Plugin`. The two e2e tests that drove a plugin
  directly keep an `Arc` to it and swap it into `builtin()`'s list. The
  unit test kit no longer builds `builtin()` either: `testing::handle()`
  is a core without plugins, and `handle_with_plugin(p)` runs only the
  plugin under test and returns it. No plugin's unit tests needed another
  plugin.
- **Not done, on purpose:** the core's HTTP handlers stay in `api.rs`
  rather than moving to a `core/http.rs` as §5.1 sketched. They return
  `ApiProblem`, which lives in `api` for plugins to share, so moving them
  would add a core → api edge to save a file. `Core::new` still takes nine
  arguments; a builder would read better but touches every test. The
  "deliberately not a plugin system" notes the plan mentions were already
  gone with the fixed table in phase 4; ARCHITECTURE §2 now describes the
  plugin system instead.
- **No wire change.** Routes, JSON, events, error codes and the identity
  packet are the same; Rust callers change shape (`ferry::core`,
  `ferry::daemon`).
- **Checked live**, isolated (a temporary data and download dir each, free
  ports, loopback discovery, a private Xvfb display and D-Bus session):
  the app, a CLI peer, and the fake phone. Pairing an incoming request
  from the peer in the app (codes matched), ping both ways, clipboard
  both ways (through the app's X clipboard, read and written by a third
  daemon with `--system-clipboard` on the same display), a 5 MB file both
  ways through the file picker and `ferry send` (byte-identical),
  pairing the fake phone from the app's Add device page, its battery
  (`Connected · 73%`), browsing its storage and downloading a file,
  ringing it from the tray menu, and Quit from the tray. `ss -lunpt`
  showed only `127.255.255.255:1716` and `127.0.0.1` listeners for every
  process started.

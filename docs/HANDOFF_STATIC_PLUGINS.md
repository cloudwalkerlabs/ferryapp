# Handoff: replace the erased plugin registry with a fixed enum

Status: planned; implementation has not started. Written on 2026-09-29 for
[PR #76](https://github.com/simophin/ferryapp/pull/76), branch
`async-sqlite-plugins`. This document is the next task, not a description
of changes already made in that PR.

Read [HANDOFF.md](HANDOFF.md), [ARCHITECTURE.md](ARCHITECTURE.md) §2,
[ADR 0003](adr/0003-use-async-sqlite-pools-and-plugin-callbacks.md), and
[../CLAUDE.md](../CLAUDE.md) first. Those contain the project rules and
isolation requirements. Start from the latest PR branch or its merged
result; do not reconstruct the async SQLite migration.

## Owner's direction and scope

The owner questioned the new `async-trait` dependency, then the need for
`Arc<dyn Plugin>`: Ferry has a fixed set of built-in features and does not
need dynamic plugin loading. They requested a glimpse of a fixed enum
approach and this detailed handoff for the next agent.

The proposed direction is a manually exhaustive listing of the concrete
plugins, with native async dispatch and no erased plugin objects. Keep
shared ownership of concrete instances. The owner has requested the
handoff here; this documentation change does not implement the migration.

The next implementation should:

- Replace the production `Vec<Arc<dyn Plugin>>` with a fixed plugin enum.
- Remove `async-trait` from the plugin contract and implementations, and
  remove the direct Cargo dependency once there are no remaining uses.
- Preserve async callbacks, ordering, cleanup, snapshots, HTTP routes,
  capability advertisement and the current typed feature APIs.
- Keep the plugin list in one composition module. Core operations should
  continue to call a common interface rather than naming individual features.
- Preserve every behavioral regression introduced in PR #76.

Keep this work focused on representation and dispatch. Database pooling,
protocol behavior, feature behavior, UI design, and backend trait objects
are separate concerns. Do not replace every `Arc` or every `dyn` in the
project. In particular, `Arc<dyn ClipboardService + Send + Sync>` remains
useful for choosing the desktop or in-memory backend.

## Baseline already implemented

PR #76 currently uses `#[async_trait::async_trait]` on `core::Plugin` and
its implementations. `handle_packet`, `connected`, `paired`,
`disconnected`, `unpaired`, and `started` are async. `shutdown` still
returns `BoxFuture`. Metadata, routes and device snapshots are synchronous.

Other completed changes must remain intact:

- SQLite runs on blocking workers behind `deadpool-sqlite`: WAL, one writer
  and three readers for file databases; one shared connection for memory tests.
- Transactions commit, update the config cache and notify watchers in the
  writer closure, including after cancellation of the awaiting caller.
- No-op writes refresh the process-local config cache after external writes;
  deleted config keys are evicted. Snapshots read memory only.
- Pools retain a Tokio handle for destruction outside the runtime, including
  when the last store reference drops on the UI thread.
- Per-device async gates order callbacks and lifecycle mutations. Core state
  locks are brief and never held across plugin awaits.
- Tracked settings and trust mutations finish persistence and memory effects
  after caller cancellation. Shutdown drains them before plugin shutdown.
- LAN socket reading, bounded packet dispatch and writing run independently.
  Shutdown cancels the active packet callback before disconnect cleanup.
  Socket EOF allows a bounded one-second drain for the peer's last unpair packet.
- Discovery tie-breaking replaces pending handshakes only; authenticated
  connections are preserved. Cleanup of an old connection checks its token
  before touching a replacement.
- Pairing test subscriptions are established before awaited requests can
  publish events.

The final implementation baseline passed 565 tests across 19 test targets,
workspace Clippy with warnings denied, formatting, the CLI build with zero
iced dependencies, and diff checks. The sharing/pairing integration test
passed eight consecutive fresh-connection runs after the discovery fix.
`cargo-about` was unavailable locally; new dependency license metadata was
checked, and CI performs the complete license audit. Revalidate the next
implementation rather than treating these results as evidence for it.

## Proposed code shape

This is an abbreviated sketch, not copy-ready code. The full implementation
must forward every method and list all nine built-ins.

```rust
// src/plugins/mod.rs
#[derive(Clone)]
pub enum BuiltinPlugin {
    Ping(Arc<ping::PingPlugin>),
    FindMyPhone(Arc<findmyphone::FindMyPhonePlugin>),
    Battery(Arc<battery::BatteryPlugin>),
    Connectivity(Arc<connectivity::ConnectivityPlugin>),
    Clipboard(Arc<clipboard::ClipboardPlugin>),
    Share(Arc<share::SharePlugin>),
    Browse(Arc<browse::BrowsePlugin>),
    Notifications(Arc<notifications::NotificationsPlugin>),
    Telephony(Arc<telephony::TelephonyPlugin>),
}
```

An enum method matches the concrete variant and awaits that plugin's method.
Each arm can return a different concrete future because it is awaited inside
one outer async method. Avoid returning the unawaited arm futures directly:
those have different types. There is no need to box each packet callback.

```rust
impl BuiltinPlugin {
    pub async fn handle_packet(
        &self,
        ctx: &PluginContext,
        device: &DeviceSnapshot,
        packet: &Packet,
    ) {
        match self {
            Self::Ping(plugin) => plugin.handle_packet(ctx, device, packet).await,
            // Explicit equivalent arms for the other eight variants.
        }
    }
}
```

Retain the existing packet index and loops in the core:

```rust
// src/core/plugin.rs
pub struct PluginRegistry {
    plugins: Vec<BuiltinPlugin>,
    by_packet_type: HashMap<&'static str, usize>,
}

impl PluginRegistry {
    pub fn for_packet(&self, packet_type: &str) -> Option<&BuiltinPlugin> {
        self.by_packet_type
            .get(packet_type)
            .map(|&index| &self.plugins[index])
    }

    pub async fn disconnected(&self, ctx: &PluginContext, device_id: &str) {
        for plugin in &self.plugins {
            plugin.disconnected(ctx, device_id).await;
        }
    }
}
```

The call in `Core::handle_peer_packet` remains almost unchanged:

```rust
if let Some(plugin) = self.plugins.for_packet(&packet.packet_type) {
    plugin
        .handle_packet(&self.plugin_context(), &device, &packet)
        .await;
}
```

Keep the surrounding pairing special case, device gate, paired-device check,
and cancellation behavior. The snippets omit those only to show dispatch.

Start with explicit forwarding matches. A small local macro may generate
those matches from one plugin listing if the repetition warrants it. Avoid
adding a procedural macro dependency or a general plugin framework.

### Trait contract and `Send` futures

Keeping the `Plugin` trait for concrete implementations and its default hooks
is useful; it no longer needs to be dyn-compatible. Do not assume that a
native `async fn` declaration on a public trait guarantees a `Send` future.
The daemon's spawned work requires `Send`.

Choose a contract that makes this explicit, for example a native
return-position `impl Future` bound:

```rust
fn connected(
    &self,
    _ctx: &PluginContext,
    _device: &DeviceSnapshot,
) -> impl Future<Output = ()> + Send {
    async {}
}
```

Concrete implementations can use `async fn` while meeting that contract.
Apply the same reasoning to all awaited hooks, especially
`started(self: Arc<Self>, ...)` and shutdown. Another option is to make the
trait internal and prove the concrete enum futures are `Send`; explain the
choice and let the real Tokio spawn sites verify it. Do not silence the
public async-trait warning without deciding the future contract.

`routes`, `streaming_routes`, and `started` currently have `Arc<Self>`
receivers. Forward them by cloning the variant's concrete `Arc`; preserve
ownership of the same instance. `Parts.clipboard`, `.browse`, and
`.notifications` must point at the instances installed in the core.

Preserve concurrent shutdown. An enum's native async `shutdown` method gives
one concrete future type, so `join_all` can still collect those futures. It
is also acceptable to retain the existing explicit `BoxFuture` shutdown
contract as a deliberate exception; distinguish that from the removal of
per-packet boxing. Do not remove `futures-util` merely because
`async-trait` is removed; it has other uses.

### Core/composition boundary

The example introduces a dependency from `core::PluginRegistry` to a type
owned by `plugins`. That is a module-boundary change: the old core accepts
arbitrary implementations without depending on the built-in composition.
Keep the enum and individual feature imports in `plugins`; core code should
only know the aggregate enum and common contract. Record this tradeoff in
a new ADR rather than claiming the old boundary is identical.

Avoid making `Core`, transports, API state, UI context and every caller
generic merely to preserve arbitrary plugin injection. If a different
boundary is chosen, explain why its complexity is justified.

## Migration map

| Area | Current seam | Required work |
| --- | --- | --- |
| `src/plugins/mod.rs` | `builtin`, `builtin_parts`, `Parts.core` return/store erased objects | Define the enum and forwarding, construct concrete variants, preserve shared UI handles and built-in order. |
| `src/core/plugin.rs` | `Plugin`, `PluginRegistry`, `for_packet`, lifecycle/routes/snapshot loops | Use the native contract and enum collection; preserve duplicate ID/type checks and concurrent shutdown. |
| `src/core.rs` | `Core::new` accepts `Vec<Arc<dyn Plugin>>` | Accept the concrete collection; keep core state and task tracking unchanged. |
| `src/daemon.rs` | `RunningService::start_with` factory returns erased plugins | Update its factory result type; preserve the GUI's composition hook. |
| `gui/src/main.rs`, `src/ui/features/mod.rs` | GUI keeps concrete handles from `Parts` | Verify pointer sharing and typed feature access still work; avoid new feature-specific core APIs. |
| `src/plugins/*/mod.rs` | `async_trait` implementations | Remove attributes and implement the native async/`Send` contract. |
| `src/core/testing.rs`, registry tests, `tests/lan.rs` | Helpers and custom probes accept arbitrary `P: Plugin` | Replace injection with concrete variants and preserve the same behavioral assertions. |
| `Cargo.toml`, `Cargo.lock`, architecture docs | Direct `async-trait` dependency and recorded erased registry | Remove the direct dependency/unused lock entries and document the final design. |

Keep metadata and packet capabilities identical, including the ordering of
lifecycle calls. Unknown packet types remain ignored; duplicate incoming
packet claims and duplicate IDs remain rejected. Keep route merging,
streaming route limits and authentication behavior.

## Test injection is the main design task

Production only needs built-ins, but existing tests depend on arbitrary
plugin implementations. Resolve this early, before changing every caller.

- Most feature tests call `handle_with_plugin` with a real built-in. Convert
  their concrete `Arc<P>` into the corresponding enum variant, potentially
  through explicit `From<Arc<P>>` implementations. Retain returning the
  concrete handle for assertions.
- `src/core/plugin.rs` defines `Claims`, `Waver` and `WaitingPlugin` to test
  registry rejection, capability checks and callback/cleanup ordering.
  Retain these assertions using concrete probe variants gated by `cfg(test)`.
  Move probe types into a reusable crate-internal testing module if needed;
  account for privacy rather than importing private sibling test types.
- `tests/lan.rs` defines `AwaitingEcho` and injects it through
  `peer_with_plugins`. It verifies callback ordering, writes while a callback
  waits, and shutdown cancelling the callback before cleanup.

A practical default is to move the custom-probe LAN regression into a
crate-internal transport test module, still using real loopback LAN services
and two cores. Then `cfg(test)` enum variants can support it without a
production escape hatch. Keep the other real-peer integration tests in
`tests/lan.rs`.

Important: an integration test builds the library without `cfg(test)`.
Adding a `#[cfg(test)]` variant alone will not make `tests/lan.rs` compile.
If a feature-gated test-support seam is chosen instead, isolate the probe
integration target and explicitly run it with that feature in CI. Do not
silently stop running the regression in the standard validation workflow.
Do not retain `dyn Plugin` in the production enum solely for tests, or
replace the real socket test with a mock-only test.

## Suggested implementation sequence

1. Confirm the current branch/base and inspect all `dyn Plugin` and
   `async_trait` uses. Keep unrelated untracked `.claude/` files untouched.
2. Decide the test-probe placement and the native future `Send` contract.
3. Add the complete enum, explicit forwarding and conversions. Keep native
   async calls directly awaited; verify concrete instance sharing.
4. Change the registry and composition signatures, then feature fixtures and
   probe tests. Keep validation and lifecycle behavior in the same places.
5. Remove all plugin `async_trait` attributes and the direct dependency.
   Consider shutdown separately and preserve its concurrency.
6. Run the regression suite and required checks below. Fix races through
   ordering and ownership; avoid increasing timeouts to conceal failures.
7. Add a new ADR superseding only the erased dispatch/`async-trait` portion
   of ADR 0003. Update ARCHITECTURE §2, HANDOFF's plugin registration rule,
   and this document's status. Accepted ADR history should remain intact.
8. Put the implementation in the agreed PR/branch, with validation results.
   Archive this handoff once complete and link its replacement ADR.

## Validation and completion criteria

Run under a private display and D-Bus session as CLAUDE.md requires:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
env -u WAYLAND_DISPLAY dbus-run-session -- xvfb-run --auto-servernum \
  cargo test --workspace --all-targets --locked
cargo build -p ferry --locked
cargo tree -p ferry -e normal --prefix none --locked
# The normal dependency tree must contain no iced crates.
git diff --check
rg -n 'dyn Plugin|async_trait|async-trait' src tests Cargo.toml
```

Inspect remaining search hits rather than removing unrelated trait objects.
`async-trait` may remain in Cargo.lock through third-party dependencies;
that does not mean Ferry's plugin dispatch still uses it. Confirm there is
no direct dependency or plugin attribute.

Retain and run these regressions, even if their locations change:

- `a_store_can_drop_outside_its_runtime`
- `a_slow_writer_does_not_block_the_runtime_or_readers`
- `a_noop_write_refreshes_the_cache_after_an_external_write`
- `cancellation_does_not_lose_commit_notifications`
- `cancelled_settings_call_still_updates_memory_and_persistence`
- `cleanup_follows_an_in_progress_callback_without_blocking_snapshots`
- `stale_connection_cleanup_preserves_its_replacement`
- `tie_breaking_replaces_pending_handshakes_but_preserves_live_connections`
- `awaiting_callbacks_preserve_order_and_allow_socket_writes_and_shutdown`

Verify the existing capability advertisement test, duplicate registry
validation, shared UI/core plugin state, pairing/unpairing, clipboard,
sharing, notifications, browsing and UI end-to-end behavior. If changing
any test feature gates, demonstrate both the default suite and the extra
probe suite run in CI. Recheck licenses with `cargo-about` where available.

Completion means the registry is concretely exhaustive, plugin callback
futures no longer depend on `async-trait`, shared concrete instances and
all async lifecycle guarantees remain correct, and documentation matches
the final implementation. Do not mark the task complete merely because the
enum compiles or because the custom-probe tests were removed.

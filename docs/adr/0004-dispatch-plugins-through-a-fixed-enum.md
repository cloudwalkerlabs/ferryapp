# 0004. Dispatch plugins through a fixed enum

- Status: Accepted
- Date: 2026-09-29
- Supersedes: the `async-trait` and `Arc<dyn Plugin>` dispatch in [0003](0003-use-async-sqlite-pools-and-plugin-callbacks.md)

## Context

[0003](0003-use-async-sqlite-pools-and-plugin-callbacks.md) made plugin
callbacks async with `async-trait`, which boxes every callback future so the
core could hold `Vec<Arc<dyn Plugin>>`. Ferry's features are fixed at compile
time: nothing is loaded at runtime, so the erasure only served arbitrary
implementations injected by tests.

## Decision

`plugins::BuiltinPlugin` is an enum with one variant per built-in plugin,
each holding the plugin's `Arc`. A `builtin_plugins!` macro in
`src/plugins/mod.rs` lists the plugins once and generates the enum, its
`From<Arc<T>>` conversions and one forwarding method per `Plugin` method.
Each async method awaits the concrete plugin's future inside one outer
future, so no callback is boxed. `PluginRegistry` and `Core::new` take
`Vec<BuiltinPlugin>`; the GUI still keeps its own `Arc`s of the same
instances through `builtin_parts`.

The `Plugin` trait stays as the contract for the concrete plugins, with
native `impl Future<Output = ()> + Send` hooks so the daemon's spawned work
is checked at compile time. `shutdown` keeps returning `BoxFuture`, so the
enum collects every plugin's shutdown for `join_all`; that is one box per
plugin at exit, not per packet. The `async-trait` dependency is removed.

The enum has no test-only variants. Registry validation is a plain function
(`index_packet_types`) tested with tuples. Ordering and cancellation tests
drive the real clipboard plugin through a `ClipboardService` whose `set`
blocks, which holds the packet callback open, including over real sockets in
`tests/lan.rs`.

## Consequences

`core::PluginRegistry` now depends on a type owned by `plugins`, where it
accepted any implementation before. This boundary change is deliberate: the
core still names no feature and calls one common interface, but it knows the
aggregate enum. Nothing above the core became generic.

A new plugin is one module, one line in `builtin_plugins!` and one line in
`builtin_parts`. Adding a `Plugin` method means adding its forwarding line
to the macro. Tests can no longer inject an arbitrary plugin: a scenario
that needs a callback held open has to find a seam in a real plugin, as the
clipboard backend is. Cleanup state such as a device-scoped value cleared
in `disconnected` cannot be observed that way unless the real plugin keeps
one.

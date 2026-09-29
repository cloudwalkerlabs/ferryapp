# 0003. Use async SQLite pools and plugin callbacks

- Status: Accepted
- Date: 2026-09-29
- Supersedes: the synchronous store access in [0002](0002-store-the-daemons-data-in-sqlite.md)

## Context

Even a small SQLite query can wait for filesystem I/O or another process's
transaction. Running it under a shared synchronous connection lock can
stall the UI and Tokio workers. Synchronous plugin callbacks also force
features to spawn tasks for routine I/O, complicating ordering and cleanup.

## Decision

Keep bundled `rusqlite`, the schema and existing migrations. Add
`deadpool-sqlite` 0.14, compatible with `rusqlite` 0.40, to acquire
connections asynchronously and run SQL closures on blocking workers.
File databases use WAL, a single writer pool and a separate pool of three
readers. Each pool retains the daemon's Tokio handle for closing SQLite
connections when the last store reference drops outside the runtime (for
example on the UI thread). Every connection enables foreign keys and a five-second busy
timeout. In-memory tests share one connection, so every query sees the
same database.

Make store opening, queries, mutations and watch registration async.
An entire transaction, its commit, config cache update and notifications
execute in one writer closure; cancellation of its waiter cannot lose
post-commit notifications. Watch registration uses the writer so its
initial read cannot miss a concurrent commit. Closures own their inputs,
are synchronous and must not re-enter the store.

Expose memory-only config snapshots through `Store::cached`. These and
config watches reflect this store's writes; callers needing another
process's latest value await `get`. TLS loads pins before entering the
synchronous certificate verifier and refuses the connection on lookup
failure. UI snapshots remain synchronous and perform no database I/O;
mutations run on the daemon's runtime.

Make packet handlers and startup/connection/pairing/cleanup hooks async
with `async-trait` 0.1, retaining metadata, routes and snapshots as
synchronous methods. Keep the existing boxed async shutdown hook.
Serialize device callbacks and lifecycle changes through per-device async
gates. Socket reading, bounded packet dispatch and writing run independently,
so an awaited callback can still send packets. Disconnect cancels the
active packet callback before cleanup, allowing queued packets a bounded
one-second drain on socket EOF so a final unpair packet is not lost.
Explicit shutdown preempts that drain. Discovery tie-breaking only replaces
pending handshakes, and stale cleanup cannot remove a newer connection.
Core trust mutations and settings
updates, including plugin settings, use tracked tasks to finish persistence and memory effects after
caller cancellation; daemon shutdown drains those tasks before plugin
shutdown.

## Consequences

The Rust store/core mutation API and plugin implementations now require
awaits. HTTP resources and database format stay the same. Read/write pool
bounds limit worker use; WAL permits readers during a write. SQLite can
still wait for external writers, but that wait occurs off the UI and
Tokio workers.

Plugin callbacks must be cancellation-safe, keep synchronous locks out of
awaits, and avoid awaiting lifecycle changes or replies that need the same
device dispatcher. Long-lived tasks remain plugin-owned and need explicit
cleanup. Shutdown hooks must finish promptly. Async callbacks simplify
routine I/O but do not make blocking filesystem or platform APIs async;
those still require `spawn_blocking` (as remote clipboard writes do).

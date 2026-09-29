# What to build next

> **Archived (2026-09-24).** Items 1–9 and 11 are done, and item 10 is
> built. This copy keeps the detail behind them: what was built, how it
> was checked, and the traps found along the way. The current handoff,
> with the ground rules and the open work, is
> [`../HANDOFF.md`](../HANDOFF.md).

Handoff for agents continuing MyConnect after the first Flutter UI milestone
(2026-09-24). Each item below is self-contained: why it matters, where things
stand today (with file pointers), what to build, how to know it's done, and
the traps we already know about. Work top to bottom unless the user says
otherwise; items within a priority band are independent.

## Read first

1. [`ARCHITECTURE.md`](../ARCHITECTURE.md): module map, state machines, the
   full HTTP API, the FFI embedding (§9), known gaps (§11).
2. [`../ui/README.md`](../../ui/README.md): how to run the app, including two
   instances on one machine.
3. [`../ui/docs/adr/`](../../ui/docs/adr/README.md): why the UI is shaped the
   way it is. ADR 0001 and 0003 are the ones you will lean on.

## Ground rules (set by the project owner)

- **The UI is dumb.** It persists nothing and reads/writes only through the
  HTTP API. If a feature needs data the API doesn't have, add the endpoint
  (and, if the data changes over time, an event) in Rust first.
- **Every resource needs a snapshot endpoint and events.** The UI loads a
  snapshot, patches it from `/events`, and refetches after any reconnect
  (ADR 0003). A resource with events but no list endpoint (or the reverse)
  leaves the UI unable to recover after a gap.
- **Native code only starts and stops the daemon** (`ffi/`, ADR 0002). Don't
  add FFI functions for application features.
- **Token auth is optional.** It is enforced only when the daemon was
  started with one. The FFI always sets one; the CLI defaults to none.
- Use reputable dependencies, and record new ones in ADR 0005's table (or
  write a new ADR if the choice changes an existing decision).
- Update `ARCHITECTURE.md` when the API or module map changes.

Done means all of these pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
(cd ui && dart run build_runner build --delete-conflicting-outputs \
       && flutter analyze && flutter test && tool/integration_test.sh)
git diff --check
```

Also run the real app for any UI change (see "Verifying in the real app" at
the end). Unit tests with a fake daemon host missed two real bugs in the
first milestone.

## Where things stand

Working and verified live (UI ↔ CLI daemon over loopback):

- Device list with live reachability, device details, unpair.
- Add device: scan, list nearby unpaired devices, start pairing, show the
  verification code and the outcome.
- Incoming pairing prompt on any screen, which clears itself when the request
  is resolved elsewhere or expires.
- An embedded daemon started through FFI on a free port with a per-launch
  token, and a "reconnecting" banner if the event stream drops.
- Close to tray, tray Show and Quit, pairing-request notifications, and a
  single instance on Linux (item 3).
- Sending a file from a device's page, a transfers view with progress,
  cancel, and open file/folder, and a notification for received files
  (item 4). Files dropped on the window go to the device they were dropped
  on or to one chosen in a dialog.
- A tray menu with each paired device (send files, ping, show details),
  Settings and Quit; a left click on the icon shows the window. Linux
  needs a patched `cnativeapi`, vendored in `ui/third_party/`.
- A settings screen (device name, download folder, clipboard sync, close
  to tray), stored by the daemon, with renames reaching peers at once
  (item 5).
- The desktop clipboard synced with peers, in both directions (item 6).
- End-to-end tests of the real app against a CLI peer (item 7).
- Ping both ways: a Ping button on the device page, and received pings as a
  snackbar or a notification (item 8).
- Browsing a paired Android device's files: list, preview images,
  download, upload (picked or dropped), new folder, rename, delete, from
  the app and `myconnect files` (item 11). Checked from the CLI against
  a Pixel 8a, and in the app against a fake phone.

Only Linux bundles the native library. Nothing has been tested against a real
KDE Connect install yet.

## Priorities at a glance

| # | Item | Priority | Mostly |
| --- | --- | --- | --- |
| 1 | ~~[Tell the peer when unpairing](#1-tell-the-peer-when-unpairing)~~ **Done** | P0 | Rust |
| 2 | ~~[Interop check against real KDE Connect](#2-interop-check-against-real-kde-connect)~~ **Done** | P0 | Manual + Rust fixes |
| 3 | ~~[Keep running in the background](#3-keep-running-in-the-background-tray-and-notifications)~~ **Done** | P0 | Flutter |
| 4 | ~~[Send files and a transfers view](#4-send-files-and-a-transfers-view)~~ **Done** | P1 | Flutter (+ small API) |
| 5 | ~~[Daemon settings API and screen](#5-daemon-settings-api-and-settings-screen)~~ **Done** | P1 | Rust + Flutter |
| 6 | ~~[OS clipboard integration](#6-os-clipboard-integration)~~ **Done** | P1 | Rust |
| 7 | ~~[Automated end-to-end test](#7-automated-end-to-end-test)~~ **Done** | P1 | Test infra |
| 8 | ~~[Ping: send button and receiving](#8-ping-send-button-and-receiving)~~ **Done** | P2 | Flutter + Rust |
| 9 | ~~[Add device by IP address](#9-add-device-by-ip-address)~~ **Done** | P2 | Rust + Flutter |
| 10 | [macOS and Windows packaging](#10-macos-and-windows-packaging) | P2 | Build |
| 11 | [Browse a device's files](#11-browse-a-devices-files) **Built; checked on a Pixel 8a from the CLI** | P2 | Rust + Flutter |

---

## 1. Tell the peer when unpairing

> **Done (2026-09-24).** `forget_device` queues `pair: false` before
> cancelling the connection, and the LAN connection loop now flushes queued
> packets on cancel (bounded by `CLOSE_FLUSH_TIMEOUT`). A `pair: false` from
> a paired peer outside a pairing session removes trust and publishes
> `device.updated` with `paired: false`, keeping the connection open. Covered
> by unit tests in `src/application/service.rs` and the unpair step of
> `tests/pairing_e2e.rs`. See ARCHITECTURE §4.

**Why.** Unpairing is one-sided today. If A unpairs B, B still shows A as
paired, reconnects, and has trust that A no longer honours. We saw this live:
the CLI unpaired the app, and the app kept listing the CLI as paired.

**Current state.** Both directions are broken:

- *Sending.* `ApplicationHandle::forget_device`
  (`src/application/service.rs`) removes trust, forgets the device, cancels
  the connection and emits `device.forgotten`, but never sends
  `kdeconnect.pair {"pair": false}`.
- *Receiving.* `handle_pair_body` handles `pair: false` only when a pairing
  session is in progress (it looks up `pairing_by_device` and returns early
  otherwise). A `pair: false` from an already-paired peer is ignored.

**Build.**

- In `forget_device`, if the device has a live connection, send
  `kdeconnect.pair {"pair": false}` on it *before* cancelling the
  connection. The sender is `try_send` into a bounded channel, so give the
  writer a moment to flush (or add a "send then close" path on the
  connection) rather than cancelling in the same instant.
- In `handle_pair_body`, when `pair` is false and there is no active pairing
  but the device is trusted: remove trust from the `TrustStore`, set
  `paired = false` in the registry, and publish `device.updated`. Decide
  whether to also drop the connection; KDE Connect keeps it open but unpaired.
- The UI needs no change: `device.updated` with `paired: false` moves the
  device out of the home list and into "Add device".

**Done when.** A new e2e test in `tests/pairing_e2e.rs` pairs two peers,
unpairs from one side, and asserts that the other side reports
`paired: false` and has no trust entry. In the app, unpairing from the CLI
removes the device from the app's list without a restart.

---

## 2. Interop check against real KDE Connect

> **Done (2026-09-24), against KDE Connect for Android.** Checked with the CLI
> daemon (real LAN, separate data dir) against the owner's Pixel 8a. Every
> step works both ways except ping from the phone, which is item 8. Fixed along
> the way: `ApiClient::send_file` didn't set the file part's `Content-Length`
> header, so `myconnect send` always failed with `missing_declared_size`.
> Incoming pair requests were checked against the 30 s pairing timeout
> instead of KDE Connect's 1800 s clock-skew tolerance, so a phone whose clock
> was 2 minutes off couldn't pair with us. Dropped pair requests and received
> packet types are now logged at debug level. The phone only sends its
> clipboard when the user taps "Send clipboard" (an Android 10+
> restriction). Each UDP announcement from a connected peer makes us re-dial
> and replace the session; upstream does the same (rate-limited per device),
> so this is expected. Still open: a KDE Connect *desktop* peer, and a pass
> through the Flutter app instead of the CLI. See ARCHITECTURE §11.

**Why.** Every test so far is MyConnect against MyConnect. The protocol code
follows the research in [`archive/KDECONNECT_PROTOCOL_RESEARCH.md`](archive/KDECONNECT_PROTOCOL_RESEARCH.md),
but no real KDE Connect peer has been exercised. A phone running KDE Connect
("Pixel 8a") was visible on the owner's LAN during the last session, so this
can be done now. **Ask the user before pairing with their real devices.**

**Build / do.**

- Run the app *without* `MYCONNECT_DISCOVERY_LOOPBACK` so it announces on the
  real network. Use a separate `MYCONNECT_DATA_DIR` so the session doesn't
  touch the user's identity.
- Check, in this order: discovery both ways, TLS handshake (watch for
  certificate or verification errors in the log), pairing in both
  directions with matching codes, clipboard sync, file send and receive,
  ping (the phone should show a notification), and unpair (after item 1).
- Log each failure as a gap in `ARCHITECTURE.md` §11 or fix it. Expect
  surprises in identity fields, capability names, and payload transfer
  negotiation.

**Done when.** `ARCHITECTURE.md` §11 no longer says interop is untested, or
lists precisely what doesn't work.

---

## 3. Keep running in the background (tray and notifications)

> **Done (2026-09-24), on Linux.** Closing the window hides it
> (`window_manager`). The tray icon (`tray_manager` 0.7, a D-Bus
> StatusNotifierItem, so no libappindicator) offers Show and Quit, and Quit
> stops the daemon before the process exits. Pairing requests that arrive
> while the window is unfocused raise a notification
> (`flutter_local_notifications`). The notification is withdrawn when the
> request is resolved, and clicking it brings the window back with the
> prompt. The Linux runner is a unique `GApplication`, so a second launch
> shows the running window and exits. The policy is in
> `lib/src/features/background/background_host.dart`; the plugins sit behind
> `DesktopShell` and `DesktopNotifications` (`lib/src/core/desktop/`), faked
> in widget tests. Verified live under Xvfb with a private D-Bus session, a
> stand-in notification server and a stand-in tray host: close, then a CLI
> pairing request, a notification, click, prompt, cancel from the CLI (the
> notification closes), a second launch, then tray Show and Quit (API and LAN
> ports closed, the peer sees the device unavailable). See ADR 0007. Not done:
> start on login (it needs item 5's settings), a Windows single-instance
> mutex (item 10), and a check on a real desktop panel (KDE, or GNOME with
> the extension). Items 4 and 8 can notify through `desktopNotificationsProvider`.

**Why.** The daemon lives inside the app process (ADR 0002). Closing the
window currently exits the app, which stops the daemon, so devices lose
connectivity and incoming pairing requests or files go unnoticed. For a KDE
Connect-style app, "running" has to mean "running in the tray".

**Current state.** `lib/src/app.dart` stops the daemon in
`AppLifecycleListener.onExitRequested`. There is no tray, no notifications,
and no single-instance guard.

**Build.**

- *Close to tray.* Closing the window hides it; a tray menu offers
  Show and Quit. Only Quit (or a real OS shutdown) stops the daemon and
  exits. Candidate packages: `window_manager` (intercept close and hide the
  window) and `tray_manager` (tray icon and menu), both widely used on
  desktop. Check their current Linux support (AppIndicator on GNOME needs
  the extension) and record the choice in ADR 0005.
- *Desktop notifications* for: an incoming pairing request (clicking it shows
  the window, where the prompt is already displayed), a received file (item
  4), and a received ping (item 8). Candidate: `flutter_local_notifications`,
  which supports Linux, macOS and Windows. Drive notifications from the same
  controllers the UI uses (e.g. listen to `pendingIncomingPairingsProvider`
  for new ids), not from a second event subscription.
- *Single instance.* A second launch should focus the running window rather
  than start a second daemon, which would fight for UDP 1716 and create a
  confusing second identity if the data dir differs.
- *Start on login* (optional): an autostart `.desktop` entry on Linux. It is
  a UI setting that has to live in the daemon (item 5), per ground rule 1.
  Base it on the entry `install.sh` writes to
  `~/.local/share/applications` (from `ui/linux/packaging/`), which has the
  bundle's absolute `Exec` path.

**Pitfalls.** Keep daemon shutdown on the real quit path, and keep the
`ProviderScope` alive while the window is hidden, since disposing it stops the
daemon through `daemonEndpointProvider`.

**Done when.** The app can be closed to the tray, still accepts a pairing
request (notification, then prompt), and Quit stops the daemon cleanly (the
API port stops listening).

---

## 4. Send files and a transfers view

> **Done (2026-09-24).** The device page has a "Send file" button
> (`file_selector`), enabled when the peer is connected and lists
> `kdeconnect.share.request`, and shows that device's five most recent
> transfers. `/transfers` lists all of them. Each row shows direction,
> progress and outcome, with Cancel while running and Open file/Open folder
> (`url_launcher`) once received. `TransfersController` follows the pairings
> pattern and never lets a stale upload response overwrite a newer event.
> Received files raise a notification while the window is unfocused. Daemon
> changes: completed incoming snapshots carry `savedPath` (absolute), so no
> download directory is needed in `/status`. The upload route no longer
> sits under the 15 s request deadline (the "verify first" worry was real:
> a new test in `tests/api.rs` fails without the fix), and fails with
> `request_timeout` only when the upload stalls for that long.
> `transfer.progress` events are throttled to one per 100 ms per transfer,
> since one per 64 KiB chunk could overflow the 256-slot event bus and drop
> the UI's stream. Verified live under Xvfb against a CLI peer: 50 MB each
> way, byte-identical; a 2 GB upload still running after 20 s, then
> cancelled from the app (the partial file was removed on the peer); Open
> folder launched the file manager on the download directory. Not done:
> drag-and-drop onto a device (`desktop_drop`), sending several files at
> once (the API takes one file per request), and a live check of the
> received-file notification (it is covered by a widget test).
>
> **Later (2026-09-24):** drag-and-drop and several files at once are
> done, in the UI only: `FileDropZone` (`features/send/`, `desktop_drop`)
> sends files dropped on a device tile or page straight to it, and asks
> which device for a drop anywhere else; "Send file" and the tray's new
> "Send files…" pick several files. Files go one request at a time, in
> order. The tray icon itself can't take drops: StatusNotifierItem (Linux)
> and the Windows notification area have no drop support, and only macOS's
> `NSStatusItem` could, through native code. Verified live under Xvfb
> against a CLI peer, with a GTK drag source driven by XTest: a drop on the
> peer's tile, two files dropped on empty space and sent through the dialog,
> and the tray entry (clicked through `com.canonical.dbusmenu.Event` on the
> private bus) all arrived byte-identical. Not checked: a drop from a real
> file manager, macOS and Windows.

**Why.** File transfer is a core feature and the API already supports it end
to end; only the UI is missing.

**Current state (API).**

- `POST /api/v1/transfers`: streaming `multipart/form-data` with a
  `deviceId` text field **first**, then one `file` part. The `file` part
  **must carry its own `Content-Length` part header** (the declared size is
  checked against the configured maximum before anything is sent). See
  `post_transfer` in `src/api.rs`.
- `GET /transfers`, `GET /transfers/{id}`, `DELETE /transfers/{id}` (cancel).
- Events: `transfer.started/progress/completed/failed`. The UI currently
  decodes these as `UnhandledEvent` (`lib/src/core/api/models/event.dart`).
- Only paired, connected devices can send or receive.

**Build.**

- *Models and API:* a Freezed `Transfer` model mirroring `TransferSnapshot`,
  and `transfers()`, `sendFile(deviceId, path)` and `cancelTransfer(id)` in
  `MyConnectApi`. For the upload, use `MultipartFile.fromFile(path,
  headers: {'content-length': ['$length']})` and add the `deviceId` field to
  `FormData` before the file. Give this request no receive timeout, or a
  generous one; the default dio options would cut off big files.
- *Controller:* `TransfersController` following the devices/pairings pattern
  (snapshot, upsert on `transfer.*` events, refetch on reconnect). Progress
  events can be frequent; they're full snapshots, so upserting is cheap,
  but consider throttling rebuilds.
- *UI:* a "Send file" action on the device page (file picker: the official
  `file_selector` package), optional drag-and-drop onto a device
  (`desktop_drop`), and a transfers list showing direction, progress, cancel
  and final state.
- *"Open received file/folder":* the API doesn't expose where files land.
  Add the download directory to `GET /status` (or a saved path on completed
  incoming transfer snapshots), then open it with the `url_launcher` or
  `open_file` package.

**Verify first.** `enforce_request_timeout` (15 s) in `src/api.rs` appears to
wrap every route, including the streaming upload. The handler only returns
after the whole body has been forwarded to the peer, so a large or
slow-to-drain upload may be cut off at 15 s. Test with a big file to a peer
that reads slowly, and exempt the upload route if needed, the way it is
already exempted from the body-size limit.

**Done when.** A file sent from the app arrives intact on a CLI peer,
progress is visible, cancel works mid-transfer, and a file received from the
CLI shows up in the list with a working "open folder".

---

## 5. Daemon settings API and settings screen

> **Done (2026-09-24).** `settings.json` in the data dir (`config/settings.rs`,
> atomic like `trust.rs`) holds `deviceName`, `downloadDir`,
> `clipboardSyncEnabled` and the UI-owned `closeToTray`; a missing field
> means the default. `GET`/`PATCH /api/v1/settings` (a merge patch: `null`
> resets a field, unknown fields are rejected) and a `settings.changed`
> event. Precedence, decided: a start option (CLI flag or FFI config)
> overrides the stored value for that run only, and a `PATCH` of that field
> saves it and drops the override. The UI no longer sends the hostname; the
> daemon's default name is now the host name (`gethostname`, trimmed to a
> valid KDE Connect name) for the CLI too, instead of "MyConnect". A rename
> reaches peers through a `watch` channel the LAN loop follows: it
> re-encodes its identity and announces at once (a new `tests/lan.rs` test
> fails without that announcement). Incoming transfers read the download
> dir when they start. The CLI has `myconnect settings [--device-name ...]
> [--download-dir ...] [--clipboard-sync true|false]`. UI: `/settings`
> (gear on the home screen, left of Transfers), the home screen's "This
> computer" line follows the setting, and closing the window quits when
> `closeToTray` is off. Verified live under Xvfb against a CLI peer: the
> default name was the host name; an invalid name showed the daemon's
> reason in the dialog; renaming to "UI Studio" showed up in the peer's
> `scan` right away; the name and close-to-tray survived an app restart.
> Found and fixed there: pressing Enter in the rename dialog dropped focus,
> so a rejected name couldn't be retyped. Not done: start on login (it can
> now be stored as a setting), remembered manual addresses (item 9), a live
> check of the download folder picker and of close-to-tray off (both covered
> by tests), and a "use default folder" button (the API supports it with
> `null`). See ARCHITECTURE §7.

**Why.** Ground rule 1 means user preferences can't live in the UI. Today
the device name, download directory and discovery mode come only from start
options, so the user has no way to change them.

**Current state.** `RunRequest` (`src/application.rs`) takes `device_name`,
`download_dir`, etc. at start. `ApplicationHandle::set_clipboard_sync_enabled`
exists but isn't exposed over HTTP. There is no settings file.

**Build.**

- Persist a small `settings.json` in the config dir (write atomically, as
  `config/trust.rs` does). Fields to start with: `deviceName`, `downloadDir`,
  `clipboardSyncEnabled`, plus UI-owned preferences from item 3 (e.g.
  `startOnLogin`, `closeToTray`) that the daemon stores but doesn't
  interpret.
- `GET /api/v1/settings` and `PATCH /api/v1/settings`, plus a
  `settings.changed` event.
- Decide precedence and document it: explicit start options (CLI flags or FFI
  config) override stored settings for that run, or they only seed
  defaults. The FFI currently passes the hostname as the device name on
  every start, which would always override a stored name, so change the UI to
  stop sending it once settings exist.
- Renaming must reach peers: re-announce the identity (UDP) and update
  the name sent in identity packets for new connections.
- A settings screen in the UI (route `/settings`).

**Done when.** Renaming the device in the app changes the name a CLI peer
sees after a scan, and the setting survives an app restart.

---

## 6. OS clipboard integration

> **Done (2026-09-24), checked on X11.** `SystemClipboard`
> (`src/clipboard/system.rs`, over `arboard` with `wayland-data-control`)
> owns the clipboard on its own thread instead of `spawn_blocking`: it
> applies writes as they arrive and polls every 500 ms for local copies,
> which `ApplicationHandle::follow_local_clipboard` feeds into
> `set_clipboard` (the `PUT /clipboard` path). Loop guards: text the thread
> wrote is never reported back, and `set_clipboard` ignores unchanged text.
> Like KDE Connect, text already on the clipboard at start isn't synced, nor
> are empty text, images, or copies made while clipboard sync is off. Opt in
> with `RunRequest::system_clipboard` (`myconnect run --system-clipboard`,
> FFI `systemClipboard`); the app turns it on (`MYCONNECT_SYSTEM_CLIPBOARD`
> =`false` turns it off). Without a usable clipboard it logs a warning and
> falls back to `InMemoryClipboard`, which stays the default. Verified live
> with two CLI daemons on separate Xvfb displays: a copy on each side
> pasted on the other, one packet each way and no echo, `clipboard set`
> reached the desktop, a copy with sync off stayed local. The app's
> embedded daemon picked up a copy made on its display. Not done: a check on
> a Wayland compositor (with data-control, e.g. KDE, or the XWayland
> fallback on GNOME), macOS and Windows, and the optional last-synced text
> on the device page. The UI toggle already came with item 5. On X11 without a
> clipboard manager, quitting logs a harmless arboard warning that nothing
> took over the copied text. See ARCHITECTURE §6.

**Why.** Clipboard sync works over the network, but the only backend is
`InMemoryClipboard` (`src/clipboard.rs`), so nothing reaches the real
desktop clipboard.

**Build.**

- A `ClipboardService` implementation over the `arboard` crate (the standard
  cross-platform clipboard crate). It is synchronous, so call it with
  `spawn_blocking`.
- *Local change detection:* `arboard` has no change events, so poll (e.g.
  every 500 ms) and feed changes through the same path as `PUT /clipboard`.
  The feedback-loop and duplicate guards in the application core (see
  ARCHITECTURE §6) must still hold, so text just received from a peer must
  not bounce back.
- On Linux, Wayland clipboard access from a non-focused client is
  restricted. Check `arboard`'s `wayland-data-control` feature, and fall back
  gracefully (log and keep the in-memory backend) when the session doesn't
  allow it.
- Make it selectable (`RunRequest` / FFI config), keeping `InMemoryClipboard`
  for tests and headless runs.
- UI: a clipboard sync toggle (via item 5's settings) and, optionally, the
  last synced text on the device page (`GET /clipboard` plus
  `clipboard.changed`).

**Done when.** Copying text on one machine makes it pasteable on the other,
in both directions, without loops. Never log clipboard contents, only lengths.

---

## 7. Automated end-to-end test

> **Done (2026-09-24); CI runs it** (`.github/workflows/ci.yml`). `ui/integration_test/
> app_test.dart` pumps `MyConnectRoot` (now shared with `main`) with the real
> `NativeDaemonHost` and FFI library, on a fresh data dir, loopback
> discovery and an in-memory clipboard. Only notifications are silenced. Each
> test spawns its own `myconnect run` peer (`support/cli_peer.dart`: built
> with `cargo build --bin myconnect` in `setUpAll`, or `MYCONNECT_CLI`) and
> drives it over its tokenless API or `myconnect send`. Covered: incoming
> pairing accept (the codes match on both sides) and reject, outgoing pairing
> from Add device, unpair (the peer drops trust too), and a 300 KB file each
> way, byte-identical, with "Send file" answered by a fake
> `FileSelectorPlatform` instead of the GTK dialog. `ui/tool/
> integration_test.sh` runs it under `dbus-run-session` and `xvfb-run`; the
> five tests take about 8 s after the build and passed four runs in a row. A
> deliberately broken Accept button fails the first test. Waits poll in real
> time (`pumpUntil`), never `pumpAndSettle`, which spinners would hang. The
> dart-define path (`DaemonHost.fromEnvironment`) is bypassed by the host
> override, so a `dart fix` regression there would still go unnoticed. When
> a CI exists, run `tool/integration_test.sh` on Linux with Flutter, Rust,
> Xvfb and dbus installed. Not covered: settings, cancel, clipboard, the tray.

**Why.** The last milestone's two worst bugs (a daemon start crash from
isolate capture, and `dart fix` stripping `--dart-define`s) only showed up
in the real app. They were found manually.

**How the manual run worked** (reproduce it in CI):

- Start a CLI peer: `myconnect --api-port 25011 run --discovery-loopback
  --data-dir <tmp> --device-name "CLI Peer"`.
- Build the app with `--dart-define`s for loopback discovery, a temp data dir
  and a device name (see `ui/README.md`).
- Run it on a virtual display (`Xvfb :77`, `DISPLAY=:77 GDK_BACKEND=x11`).
- Drive the peer through its API. It has no token, so `curl
  http://127.0.0.1:25011/api/v1/pairings` works.

**Build.** Prefer Flutter's `integration_test` package, running the real app
with the real FFI library and pressing buttons through the widget tester, over
screenshot-and-click. Have the test spawn the CLI peer as a subprocess and
drive it with `dart:io` HTTP calls. Cover the flows: incoming
pairing accept and reject, outgoing pairing, unpair, and (after item 4) a
file round trip. Run on Linux in CI under `xvfb-run`.

**Done when.** `flutter test integration_test -d linux` passes locally and in
CI.

---

## 8. Ping: send button and receiving

> **Done (2026-09-24).** `kdeconnect.ping` is now advertised as incoming
> too, and dispatched (paired devices only) to a new `ping.received` event
> carrying `{deviceId, deviceName, message?}`; the message text is never
> logged. It is a one-off notification, so, unlike the resources, it has no
> snapshot endpoint (noted in ARCHITECTURE §8). The CLI's event printer
> formats it, but no CLI command watches for pings yet. UI: a "Ping"
> button on the device page, enabled when the device is connected and
> lists `kdeconnect.ping`, with "Pinged <name>." on success and the existing
> `ApiException` messages on failure. `BackgroundHost` follows the event hub
> directly and shows a received ping as a snackbar ("<name>: <message>", or
> "Ping!") over a focused window, or a notification otherwise. Tests: unit
> tests for dispatch and the event, `tests/ping_e2e.rs` now pings both ways
> between two MyConnect peers, widget tests for the button and both receive
> paths, and a real-app integration test that pings the CLI peer (checked
> on its `/events`) and shows the peer's ping back. Not done: sending a
> ping with a message from the app, and a recheck of phone-to-app pings
> against KDE Connect for Android (item 2 saw them dropped before this).
> Snackbars queue, so a received ping right after "Pinged <name>." waits
> for that one to close.

**Why.** It's a cheap, visible "is it working?" feature, and it's what KDE
Connect users expect.

**Current state.** `POST /devices/{id}/ping` exists (optional message). It
returns `409 unsupported_by_peer` if the peer doesn't list `kdeconnect.ping`
in its incoming capabilities, and `device_not_connected` if the peer is
offline. Incoming pings are dropped and not advertised
(`src/plugins/mod.rs`).

**Build.**

- *UI:* a "Ping" button on the device page, enabled only when the device is
  connected and its `incomingCapabilities` contains `kdeconnect.ping`. Add
  a `ping()` method to `MyConnectApi` and friendly messages for the error
  codes in `ApiException.message`.
- *Rust, receiving:* handle `kdeconnect.ping` in the plugin dispatch,
  advertise it in `plugins::capabilities()`, and publish a new
  `ping.received` event (device id and name, optional message). Update the
  capability tests.
- *UI, receiving:* show it as a notification (item 3) or a snackbar.
  Decode the new event type in `DaemonEvent.fromJson`.

**Done when.** Pinging from the app to a CLI peer succeeds, and a ping
from the CLI to the app surfaces in the UI.

---

## 9. Add device by IP address

> **Done (2026-09-24).** `POST /api/v1/discovery` takes an optional
> `{"address": "..."}`; `ApplicationHandle::announce_to` accepts only a
> unicast IPv4 address (else `400 invalid_address`) and queues
> `Command::AnnounceTo`, which `LanService` sends to that address on port
> 1716 (`LanConfig::with_peer_discovery_port` changes it for tests). The
> Add device page has an "Add by IP address" row and dialog, and the CLI
> has `myconnect scan --address <ip>`. Tests: `tests/lan.rs`
> (`a_peer_added_by_address_connects_without_broadcast`), `tests/api.rs`,
> and a widget test. Checked in the real app inside `unshare -rn` (a
> network namespace with only loopback, so broadcast is unreachable):
> entering `127.0.0.1` made the CLI peer appear, connected, in the list.
> Addresses are not remembered across restarts yet (see follow-ups).
>
> Trap for local checks: two instances on one host share UDP 1716 through
> `SO_REUSEPORT`/`SO_REUSEADDR`, and Linux delivers a *unicast* datagram
> only to the socket that bound last. Start the instance being added
> *after* the one doing the adding, or the announcement loops back to the
> sender and is dropped as its own identity. Separate machines don't have
> this problem.

**Why.** UDP broadcast discovery fails on many networks (client isolation,
VPNs, separate subnets). KDE Connect lets users add a device by IP address
for these cases.

**Build.**

- *API:* `POST /api/v1/discovery` accepting an optional
  `{"address": "192.168.1.20"}`. Without a body, keep today's broadcast.
  With one, send the identity packet by unicast UDP to that address on port
  1716. That makes the peer dial back over TCP, which is the normal KDE
  Connect flow. Validate the address, and keep the API from becoming a
  general-purpose UDP sender.
- *Transport:* `LanService` (`src/transport/lan.rs`) currently only
  announces to configured broadcast targets; add a one-shot unicast
  announce.
- *UI:* an "Add by IP address" action on the Add device page.
- Remembering manually added addresses across restarts is a daemon setting
  (item 5), not UI state.

**Done when.** With loopback discovery off, and broadcast blocked or
unavailable, entering a peer's IP makes it appear in the Add device list.

---

## 10. macOS and Windows packaging

> **Built, not yet run (2026-09-24).** The macOS build phase
> (`ui/macos/build_myconnect_ffi.sh`, universal via `lipo`) and the Windows
> CMake step exist, and `NativeBindings.open()` loads the macOS dylib from
> `Contents/Frameworks`. The sandbox entitlements now include network
> client and server, user-selected files and Downloads. `.github/workflows/
> build.yml` builds only macOS (a universal `MyConnect.app` in a DMG) when a
> release is published (or by hand) and attaches it, stamped with the tag's
> version, which Settings shows. It also builds Debian packages for amd64 and
> arm64 in a Debian 12 container (`ui/linux/packaging/build_deb.sh`) and, for tagged
> builds, an Arch Linux PKGBUILD that repackages them (`pkgbuild.sh`).
> Windows packages were dropped from the release build for now; the app
> still builds for it locally.
> The app has only an ad-hoc signature: users allow it once in System
> Settings → Privacy & Security, then it opens with a double-click.
> Nobody has launched the macOS or Windows app yet: check that the daemon
> starts, and on macOS that a download folder chosen in Settings still works
> after a restart (the sandbox forgets it without a security-scoped
> bookmark). Nothing is signed or notarized yet.

**Why.** The long-term goal is macOS, Linux and Windows. Only Linux bundles
`libmyconnect_ffi` today (ADR 0006).

**Build.**

- *macOS:* an Xcode build phase in `ui/macos/Runner` that runs
  `cargo build -p myconnect-ffi` and copies `libmyconnect_ffi.dylib` into
  `Contents/Frameworks`. Check the install name (`@rpath`), code signing,
  and the sandbox entitlements the app needs (network client *and*
  server, for UDP 1716 and TCP 1716–1764). `NativeBindings.open()` already
  looks for `libmyconnect_ffi.dylib`.
- *Windows:* the equivalent step in `ui/windows/CMakeLists.txt`, installing
  `myconnect_ffi.dll` next to the executable. Expect a firewall prompt on
  first run.
- Consider replacing the per-platform steps with a Flutter build hook
  (`hook/build.dart`), as ADR 0006 suggests. If so, supersede that ADR.

**Done when.** `flutter build macos` and `flutter build windows` produce apps
that start their embedded daemon. Until then, those platforms work only
against an external daemon (`--dart-define=MYCONNECT_API_URL=...`).

---

## 11. Browse a device's files

> **Built (2026-09-24).** KDE Connect for
> Android shares its storage over SFTP (ARCHITECTURE §12). The daemon
> requests an offer, connects with russh/russh-sftp (host key pinned to
> the peer's TLS certificate, signing in with our TLS key), and serves
> `/devices/{id}/files/...`. The app has a Browse files page (ADR 0008),
> and the CLI has `myconnect files <device> ls|get|cat|put|mkdir|mv|rm`.
> `tests/browse_e2e.rs` covers it against `tests/support/fake_phone.rs`,
> which follows Android's source (`SftpPlugin.kt`, `SimpleSftpServer.kt`)
> but is not Android. The app was checked live against
> `cargo run --example fake_phone`. The research behind the approach
> (mounting vs. an in-app browser) is in
> `docs/archive/remote-file-browsing.md`.
>
> **Checked on the owner's Pixel 8a** (2026-09-24, CLI daemon, not the
> app). The host key matched the pinned certificate, and Android accepted
> our ECDSA key, so no password was needed. It offered one root,
> `/storage/emulated/0` ("Internal shared storage"). Working: `ls` (with
> sizes and modification times), `cat`, a 3 MB `put` and `get` that came
> back byte-identical, a ` (1)` suffix on a clashing upload, `mkdir`,
> `mv`, and `file_exists` / `file_not_found` / `is_a_directory` /
> `file_permission_denied` (`ls /data`). Not tried on the phone: `rm`,
> and the app's Browse files page.

**Next.**

- *Still unchecked on a real phone* (ask the owner first; it is their
  device): `rm`, the app's Browse files page, an older RSA-keyed phone's
  host key (the unit test uses an RSA fixture), and how Android's
  `errorMessage` reads when "All files access" is missing.
- The idle timeout (5 minutes) is not covered by a test; it would need the
  timeout to be configurable.
- A recursive delete runs inside the 15-second request deadline; a very
  large tree can stop partway. A delete that runs as a background job with
  progress would fix it.
- Not offered: dragging files out of the browser (see ADR 0008),
  downloading whole folders, video thumbnails, and serving this machine's
  files to other devices (KDE Connect desktops don't either).
- `ui/integration_test/` doesn't cover browsing: its peer is the CLI,
  which serves no files. Running `examples/fake_phone.rs` as the peer
  would.

---

## Smaller known follow-ups

- Devices added by IP address are forgotten on restart. KDE Connect keeps
  a list of such addresses and announces to them periodically; the
  equivalent here is a daemon setting (a list of addresses, item 5's
  `settings.json`) that `LanService` announces to on its interval, plus a
  way to remove entries in the UI.
- `clipboard::system::tests::clearing_the_clipboard_is_not_reported` failed
  once under a full `cargo test --workspace` run and passed on every rerun
  and on its own; it looks timing-sensitive under load.

- Snapshots carry no sequence number, so an event emitted just before a
  snapshot response can briefly be overwritten by older data (ADR 0003).
  If this shows up in practice, add the event bus sequence to list
  responses (e.g. a header) and drop older events.
- The reconnecting banner and the startup error screen have not been
  exercised in the real app, only in unit tests.
- Widgets that `await` a mutation must capture the router or messenger
  beforehand, because an event can unmount them mid-await (see
  `device_detail_page.dart`). Apply the same care to new screens.
- `--dart-define` reads live only in `DaemonHost.fromEnvironment`, with an
  ignore for `avoid_redundant_argument_values`. Never run `dart fix` on
  that file without checking the diff.
- In debug builds the DEBUG banner covers the rightmost app bar action,
  which on the home screen is the Transfers button (at about x 1244–1268,
  y 14–38 in a 1280-wide window). It is there and clickable, just hidden.
  Don't mistake it for a missing widget, and use `find.byTooltip` in tests.
- `myconnect send` prints only the upload's response, which is taken once
  the last byte has been forwarded, so it ends on
  `transferring (N/N)` rather than `completed`. Waiting for the terminal
  state (or watching `/events`) would make the CLI report the real
  outcome.
- A transfer the sender cancels shows up on the receiver as `failed` with
  `connection_failed`, not `cancelled`, because the receiver only sees the
  payload connection close early. KDE Connect has no cancel notice in the
  share protocol either, so this probably stays; a UI could word it as
  "stopped by the sender" if it becomes confusing.

## Verifying in the real app

Isolate every run as [`../CLAUDE.md`](../../CLAUDE.md) describes: fresh
temporary data and download dirs, a free port, loopback discovery, and a
private display and D-Bus session.

```sh
dir=$(mktemp -d)
# peer
cargo run -- --api-port "$port" run --discovery-loopback \
  --data-dir "$dir/peer" --download-dir "$dir/peer-downloads" \
  --device-name "CLI Peer"
# app (separate identity, loopback only)
cd ui && flutter run -d linux \
  --dart-define=MYCONNECT_DISCOVERY_LOOPBACK=true \
  --dart-define=MYCONNECT_DATA_DIR="$dir/ui" \
  --dart-define=MYCONNECT_DOWNLOAD_DIR="$dir/ui-downloads" \
  --dart-define=MYCONNECT_DEVICE_NAME="UI Desktop"
```

Without a display (e.g. in an agent sandbox), run the built bundle under
`Xvfb`, take screenshots with `import -display :NN -window root out.png`, and
click with XTest (`libXtst` through Python `ctypes`). Don't open windows on
the user's own session, and don't pair with or send to real devices on
their network without asking.

Tips from the item 4 check:

- Launch the app under `env -u WAYLAND_DISPLAY DISPLAY=:NN
  GDK_BACKEND=x11 dbus-run-session -- ...`, with the environment *outside*
  `dbus-run-session`. Services the private bus starts (the file chooser
  portal, notifications) inherit the bus daemon's environment. With `env`
  inside, they get the owner's `DISPLAY`/`WAYLAND_DISPLAY`, as
  `xdg-desktop-portal` did in the item 11 check. The "Send file" picker
  opens as a GTK dialog on the virtual display. Without a window manager it
  can be bigger than the screen and doesn't get keyboard focus: move it
  on screen with `XMoveResizeWindow` and focus it with `XSetInputFocus`.
  Then press Ctrl+L, type the absolute path, and press Return, all through
  XTest key events.
- The dialog starts in "Recent" and lists the user's real recent files. Pick
  files by typed path, and don't browse or screenshot more of it than you
  need.
- Loopback transfers in a debug build run at tens of MB/s, so use a file of
  1–2 GB (from `/dev/zero`, in the scratchpad) to catch progress mid-flight
  or to cancel it.
- "Open file" and "Open folder" launch the desktop's real default app (e.g.
  Thunar) on the virtual display, and it may start helpers (`xfconfd`,
  `tumblerd`) that outlive it. Kill them afterwards, checking
  `/proc/<pid>/environ` first to make sure they belong to the private bus.
- Don't clean up with `pkill -f <pattern>`: the pattern also matches the
  shell running the command, and kills it. Kill by PID.

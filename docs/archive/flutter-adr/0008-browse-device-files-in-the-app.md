# 0008. Browse a device's files in the app, fetched on demand

- Status: Accepted
- Date: 2026-09-24

## Context

KDE Connect for Android can share its storage. On request
(`kdeconnect.sftp.request`) it runs an SFTP server and sends the port and a
one-off password (`kdeconnect.sftp`). KDE's own desktop mounts that with
`sshfs`, and its macOS build leaves the feature out. Showing the files in
the OS file manager would need a different mechanism on each platform:

- macFUSE's kernel extension, FUSE-T (which needs a commercial licence to
  bundle), or an FSKit or File Provider extension on macOS.
- WinFsp or Dokany drivers on Windows.
- FUSE or GVfs on Linux.

A mount of a phone that drops off Wi-Fi is also where file managers hang.
The research is in `docs/archive/remote-file-browsing.md`.

Every other resource is a snapshot plus events ([0003](0003-snapshot-plus-events-state-sync.md)).
A device's files can't be: nothing tells the daemon when they change on the
device.

## Decision

- **A file browser in the app, over the HTTP API.** The daemon holds the
  SFTP session (russh and russh-sftp, pure Rust apart from `ring`, which
  rustls already uses). It exposes listing, content, download, upload,
  new folder, move and delete under `/devices/{id}/files`. The UI never
  speaks SFTP ([0001](0001-stateless-ui-over-the-http-api.md)).
- **Listings are fetched, not synced.** `directoryProvider` fetches a
  folder when it is shown. It refetches when the device reconnects, after
  this app changes the folder, and on Refresh. There is no snapshot to
  patch and no event to wait for.
- **Copies are transfers.** Downloads and uploads are ordinary transfer
  resources, so progress, cancel, "open file" and the transfers view work
  unchanged, and they outlive the page.
- **Built from the framework's widgets.** No maintained Flutter file-browser
  package works with a custom backend: the popular ones take a `dart:io`
  `Directory`. The page is a `ListView` with a header row, a breadcrumb
  and popup menus. No new UI dependency.

## Consequences

- A listing can be stale if something else changes the device's files.
  The user refreshes.
- The first request to a device opens its session and can take a few
  seconds. The daemon keeps the session for five idle minutes.
- Dragging files out of the browser into the desktop's file manager isn't
  offered. On macOS and Windows it could use `super_drag_and_drop` virtual
  files; Linux has no equivalent.
- Mounting stays possible later on top of the same daemon session, e.g. a
  localhost NFS server for macOS and Linux.

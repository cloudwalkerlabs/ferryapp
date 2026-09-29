# Research: browsing a paired device's files

> Archived on 2026-09-29: the chosen in-app browser is implemented.
> Current behavior and open verification are in
> [ARCHITECTURE §12](../ARCHITECTURE.md#12-browsing-a-devices-files) and
> [HANDOFF.md](../HANDOFF.md).

Status: research (2026-09-24). Option B, the in-app browser, was then
built; see [the archived browsing ADR](flutter-adr/0008-browse-device-files-in-the-app.md)
and [ARCHITECTURE §12](../ARCHITECTURE.md#12-browsing-a-devices-files). Question: should
Ferry show another device's files through an OS mount (FUSE and
friends), or through a file browser inside the app?

## What the protocol gives us

KDE Connect has exactly one way to browse files: the `sftp` plugin. **Only
Android serves files.** kdeconnect-kde, GSConnect and kdeconnect-ios are
clients only or have no such plugin. No newer file-listing packet exists
(checked kdeconnect-android and kdeconnect-kde master, 2026-09).

| Direction | Packet | Body |
| --- | --- | --- |
| us → phone | `kdeconnect.sftp.request` | `{"startBrowsing": true}` |
| phone → us | `kdeconnect.sftp` | `{ip, port, user: "kdeconnect", password, path, multiPaths?, pathNames?}` or `{errorMessage}` |
| phone → us | `kdeconnect.sftp` | `{"serverRunning": false}` when the plugin reloads (re-request) |

We advertise `kdeconnect.sftp.request` as outgoing and `kdeconnect.sftp` as
incoming.

Notes on the Android server (`SimpleSftpServer.kt`):

- It is Apache SSHD on the first free port in 1739–1764, listening on all
  interfaces, and read-write.
- Its **SSH host key is the phone's TLS key**, which we pinned at pairing, so
  we can verify the host key properly. The reference clients use
  `StrictHostKeyChecking=no`.
- **Auth:** it accepts a public key equal to our pinned TLS certificate key.
  Our key is ECDSA P-256 from `rcgen`, which russh handles. The per-request
  password is the fallback.
- **Addressing:** use the IP of the existing LAN link, not the `ip` field,
  the same as kdeconnect-kde and GSConnect.
- **Roots:** `multiPaths` holds the roots and `pathNames` their labels. On
  API 30+ the roots are storage volumes and need `MANAGE_EXTERNAL_STORAGE`.
  Below 30 they are SAF trees.
- **Lifetime:** the server lives until the plugin unloads. Each request only
  rotates the password.

Either way the daemon needs an SFTP client. **russh 0.63 + russh-sftp 3.0**
(Apache-2.0) is pure Rust and tokio-native, with no system dependencies. Use
the `ring` backend to match rustls and rcgen, and avoid aws-lc-rs's build
tools on Windows. The other crates are worse fits: ssh2 is synchronous and
needs OpenSSL, and openssh-sftp-client needs an `ssh` binary.

## Option A: OS mount

| Platform | Best route | Effort | Friction |
| --- | --- | --- | --- |
| Linux | Hand `sftp://kdeconnect@ip:port/` to GVfs/KIO (`gio mount` / `xdg-open`) | ~1 day | Needs our key in ssh-agent or the password; host key unverified by the file manager |
| Linux | `fuser` (MIT) or localhost NFS in the daemon | Medium | None beyond code |
| macOS | Localhost NFSv3 server in the daemon (`nfsserve` crate, as `xet mount` does) + `mount_nfs -o port=N,mountport=N localhost:/ <dir>` | Medium | A stalled server can hang Finder; mount point choice may need care |
| macOS | FSKit app extension (`fskit-rs`: Swift appex ↔ Rust over localhost) | Medium–high | macOS 15.4+ (URL resources 26+), user must enable it in System Settings, tooling still rough |
| macOS | File Provider extension | High | A sync engine in Swift, files replicated into `~/Library/CloudStorage`; odd for a transient phone |
| macOS | macFUSE / FUSE-T | Low code | macFUSE kext needs Reduced Security on Apple Silicon and permission to bundle; FUSE-T needs a commercial licence to embed |
| Windows | WinFsp (GPLv3 + FLOSS exception) or Dokany | Medium | Separate driver install; WinFsp is paid if closed-source |
| Windows | Built-in WebDAV client | Low | Deprecated since 2023, off by default, 50 MB file limit. No |

A mount looks the most native, but every platform needs a different
mechanism. None of them fits the "the UI is dumb, everything goes through the
HTTP API" rule, and a mount of a phone that drops off Wi-Fi is exactly the
case where Finder and Explorer hang.

## Option B: file browser in the app

This fits the architecture. The daemon owns the SFTP session and the UI calls
HTTP. A possible API:

- `GET /devices/{id}/files?path=` returns a directory listing (the roots when
  `path` is empty).
- `GET /devices/{id}/files/content?path=` downloads, with `Range` support for
  resume and previews.
- `PUT` / `POST` upload, plus rename, delete and mkdir.
- Downloads could reuse the existing transfers resource and events, so
  progress, cancel and "open folder" come for free.

No ready-made Flutter file explorer accepts a custom or HTTP backend.
`file_manager` and `filesystem_picker` are tied to `dart:io` and stale. So
build it from maintained primitives:

- **Listing:** `two_dimensional_scrollables` (Flutter team, 0.5.4,
  2026-08). `TableView` gives a virtualized details view with a sticky
  header, and `TreeView` gives a lazily loaded folder sidebar.
- **Breadcrumbs:** hand-written, about 40 lines. The packages are stale.
- **Context menu:** `super_context_menu` (native menus) or
  `flutter_context_menu` (pure Dart).
- **Drag out to Finder/Explorer:** `super_drag_and_drop` virtual files
  produce the content on drop, so the download starts when the user drops.
  This works on macOS and Windows only; Linux gets "Download to…". Its last
  release was June 2025, which is a maintenance risk.
- **Drag in to upload:** `desktop_drop`, which we already use.
- **Image previews:** a daemon thumbnail endpoint (`image` crate) plus
  `Image.network`. Skip video thumbnails at first.

## Recommendation

1. **Build the in-app browser first.** It is one implementation for every
   OS, it follows the existing API and ADR rules, and its daemon half
   (russh-sftp session management, key auth, host-key pinning) is needed by
   any mount option anyway.
2. **Linux mount as a cheap extra:** an "Open in file manager" action that
   hands `sftp://` to GVfs/KIO.
3. **macOS mount later, if wanted:** a localhost NFSv3 server (`nfsserve`)
   over the same SFTP session. It needs no kext, licence or extension
   approval, and also works on Linux. Consider FSKit once it matures.
4. **Out of scope:** only Android can be browsed. Making *this* machine
   browsable, i.e. serving `kdeconnect.sftp` with russh-sftp's server half,
   is a separate feature.

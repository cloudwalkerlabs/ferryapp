# 0001. Build the desktop UI in Rust with iced, in the daemon's process

- Status: Accepted
- Date: 2026-09-25

## Context

The desktop app is a Flutter UI that embeds the Rust daemon through a JSON
C ABI (`ffi/`) and then talks to it only over the local HTTP API
(`ui/docs/adr/`). That costs a second language and toolchain, generated
model code mirroring the API's JSON, a Flutter build hook per platform,
and a platform plugin (often patched) for each desktop feature: tray,
notifications, drops, window placement. Every feature is written twice,
in Rust and in Dart.

A spike (`gui/`, PR #24) showed the device list in iced 0.14, running the
daemon in-process and reading the core directly, with headless snapshot
tests through `iced_test`. The owner decided to replace the Flutter app
with it. [`../archive/PLAN_ICED_UI.md`](../archive/PLAN_ICED_UI.md) is the
finished step-by-step plan; this record is the decision and the shape it
fixes.

## Decision

- **The UI is Rust and iced 0.14**, with the Lucide icon font
  (`iced_fonts`) and the Figtree text font bundled. The look follows
  iced's built-in theme palette, not the Flutter app's pixels; parity
  means the same behaviour.
- **In-process access to the core.** The app (`gui/`, binary
  `ferry-gui`) starts a `RunningService` and hands its `Core` to the UI:
  snapshots from `Core`, events from `core.subscribe()` (a fresh snapshot
  after the receiver lags), and actions through typed Rust functions. The
  UI never calls the HTTP API, but the embedded daemon still serves it,
  so the CLI can drive and inspect the instance the UI shows, and every
  feature works from the CLI.
- **The UI never blocks on the daemon's I/O.** iced runs its own executor;
  anything touching the daemon's sockets runs on the daemon's tokio
  runtime (`UiOptions::runtime`). iced's `tokio` feature stays off, so
  there is one runtime.
- **All UI code lives in `src/ui/`.** Each feature's UI is a plain module
  under `src/ui/features/` (`ping.rs`, `browse/`, …), and
  `src/ui/features/mod.rs` is the one place that lists them: one `Feature`
  message enum, and a `Features` struct whose functions (actions, status
  chips, drops, settings sections, route changes, events, demo packets)
  call each feature by name, in `builtin()` order. The rest of `src/ui/`
  is the shell: one app `Message`, typed routes (`Route::Browse`), plain
  `Task<Message>`, the pages the core owns (devices, pairing, transfers,
  settings), sync and desktop glue. `src/ui/` is a module, not a crate: a
  crate would make everything the UI touches in `core` and `plugins`
  public API. The core still never names a feature, and plugins never
  import each other. `plugins::builtin_parts()` builds each plugin once
  and also returns the instances the UI keeps (clipboard, browse and
  notifications); `RunningService::start_with` starts the daemon with
  that list.

  The first shape (2026-09-25, PR #25) put each feature's UI next to its
  plugin, `src/plugins/<name>/ui.rs`, behind a `UiPlugin` trait, so the
  UI core never named a feature. PRs #28 and #29 replaced it on
  2026-09-26: with six features all compiled in, and runtime or
  third-party plugins a non-goal, the seam bought nothing and was hard to
  read. It needed a 10-hook trait plus an erased copy, messages as
  `Arc<dyn Any>` routed by string id and downcast at runtime (a misrouted
  message panicked instead of failing to compile), a second copy of
  iced's `Task` (`Command`, `Outcome`, `ShellRequest`, each with a `map`),
  browse's folder encoded in a string route and parsed back, and a second
  plugin list (`builtin_with_ui()`) kept in step with `builtin()` by a
  test. The new shape's cost: a new feature touches a few lines in
  `features/mod.rs`, and the compiler flags a missed `match` arm.
- **A `gui` cargo feature.** `src/ui/` is behind `feature = "gui"`, which
  turns on the UI's optional dependencies; nothing in `src/plugins/` is.
  `gui/` depends on `ferry` with the feature; `cargo build -p ferry` (the
  CLI and daemon) has no iced in its tree, which CI checks.
- **Configuration is flags and environment variables**, mirroring
  `ferry-cli run`: `--data-dir`, `--download-dir`, `--device-name`,
  `--discovery-loopback`, `--discovery-port`, `--no-system-clipboard`,
  `--api-port`, `--api-token`, each also read from `FERRY_<NAME>`. The
  HTTP API is off unless Settings → Command line access turns it on
  (ARCHITECTURE §9); `--api-port` turns it on for one run. Its token is
  mandatory, made when first turned on and kept in the store
  (`core.api`); `--api-token` overrides it for one run.

### What happens to the Flutter UI's records

The Flutter app and its records stayed in force until plan step 16
deleted the app and moved them to
[`../archive/flutter-adr/`](../archive/flutter-adr/README.md). For the new
UI:

| Flutter record | For the new UI |
| --- | --- |
| 0001 Stateless UI over the HTTP API | Superseded: in-process access to the core. Carries over: the UI keeps no state of its own; its store is a cache of core snapshots. |
| 0002 Embed the daemon through a JSON C ABI | Superseded: no FFI; the daemon runs in the UI's process as a library. |
| 0003 Snapshot plus events | Carries over in spirit: subscribe, snapshot, then events, a fresh snapshot after a lag, and the same guards against stale snapshots. |
| 0004 Riverpod | Superseded: iced's update/view and a store in `src/ui/`. |
| 0005 Libraries and conventions | Superseded by the table below. |
| 0006 Build the Rust core from the platform build | Superseded: the app is one cargo binary. |
| 0007 Keep running in the tray | Carries over. |
| 0008 Browse device files in the app | Carries over; the browser calls `BrowsePlugin`'s methods instead of HTTP. |
| 0009 Remember the main window placement | Carries over: `window.json`, the UI's one piece of state. |

## Libraries

| Concern | Choice | Why |
| --- | --- | --- |
| UI toolkit | `iced` 0.14 | Pure Rust, Elm-style update/view that suits a snapshot-and-events store, multi-window `daemon` programs (a window can close while the app lives in the tray), drawn on the CPU with tiny-skia (`wgpu`, a default feature, is off: once a window had opened it kept the GPU driver stack loaded after the window closed, about 270 MB resident while the app sat in the tray, against 35 MB with tiny-skia). Pinned; upgrading is its own change. `advanced` is on for the few custom widgets (the activity bar), and `image-without-codecs` for the file browser's preview (`image::viewer` pans and zooms; codecs come from the `image` row). |
| Decoding previews | `image` 0.25 | What iced renders images with. Only the preview's codecs are on (BMP, GIF, JPEG, PNG, WebP), not every format iced's `image` feature brings. The browser decodes previews itself, off the UI thread, so an undecodable image says so ("This image can’t be shown."); iced decodes at draw time and drops the error. |
| Local time | `chrono` 0.4 | Already in the tree through `russh-sftp`, with `clock` for the local time zone (`iana-time-zone`). `time` reads the local offset only in a single-threaded process on Unix, and `jiff` would be a second time crate. |
| Icons | `iced_fonts` 0.3 (Lucide) | The Lucide icon font with typed helpers; the version matching iced 0.14. |
| Text font | Figtree (`assets/fonts/`, OFL) | Bundled, Regular and Bold only (the weights the app uses), so text looks the same everywhere and a weight missing from the system's sans-serif can't fall back to another family (a semibold title once came out in URW Bookman's serif). About 115 KB. Latin only: cosmic-text draws other scripts, such as CJK, in a system font; a bundled CJK font would be 15–20 MB a weight. The pairing code still uses the system monospace. |
| Headless UI tests | `iced_test` 0.14 | iced's own test crate. Its simulator finds and clicks widgets and renders snapshots to PNG with no display; its emulator runs the whole program, tasks and subscriptions included, for the end-to-end tests (`tests/ui_e2e.rs`). |
| Running tasks in tests | `iced_runtime` 0.14 (dev) | Already in iced's tree; its `task::into_stream` lets a unit test see what a `Task` produces, which `iced` doesn't re-export. |
| Translations | `i18n-embed` 0.16 (`fluent-system`, `desktop-requester`), `i18n-embed-fl` 0.10, `rust-embed` 8 (`docs/PLAN_I18N.md`) | The Fluent stack COSMIC, also on iced, uses. Fluent handles CLDR plurals and word order; `fl!` checks each key and its arguments against the en-US file at compile time; `rust-embed` puts `i18n/` in the binary; `desktop-requester` reads the system's languages on Linux, macOS and Windows (`sys-locale`). `fluent-syntax` (already in the tree through `i18n-embed`) reads en-US's file and writes the en-XA pseudo-locale from it at run time (`ui::i18n::pseudo`), and parses every locale's file in a test that they all have en-US's keys. |
| Numbers and dates | `icu_decimal` 2.3 (`ryu`), `icu_datetime` 2.3, `icu_calendar` 2.3, `icu_locale_core` 2.3, `fluent-bundle` 0.16 (`docs/PLAN_I18N.md`, phase 3) | ICU4X, the Unicode Consortium's CLDR formatters, compiled data included: the locale's decimal separator, grouping and digits, and its date order and 12- or 24-hour clock. `fluent-rs` writes numbers with Rust's `Display` whatever the language, so `ui::i18n::format` hands each bundle ICU4X's formatter (`set_formatter`; `fluent-bundle` is already in the tree through `i18n-embed`, as `icu_locale_core` is through `url`). chrono's `unstable-locales` has glibc's patterns, whose times all carry seconds, and no number formatting. A fixed Gregorian calendar and one field set keep the linked data small. |
| Arguments | `clap` | Already the CLI's parser; `env` reads each flag's environment variable. |
| File and folder dialogs | `rfd` | The standard native dialog crate: the XDG portal on Linux (zenity if there is none), AppKit, Win32. Its async dialogs need no runtime of their own. |
| Notifications | Linux: `zbus` 5; macOS: `mac-usernotifications` 0.3; Windows: `notify-rust` | On Linux the app talks to `org.freedesktop.Notifications` itself, so it can withdraw a notification and hear its click without a thread per notification (see Desktop integration). It uses `async-io`, which iced's theme detection already pulls in; the `tokio` feature would need a tokio runtime on iced's threads. On macOS, `UNUserNotificationCenter` through `mac-usernotifications`, by `notify-rust`'s author and the base of its (preview) macOS backend: its async API gives click and withdrawal without a thread per notification, on any runtime. `notify-rust` gives Windows toasts. |
| Opening files, folders and links | `opener` | Opens with the default app, reveals in the file manager, and opens web pages (About's links) in the browser on each platform. |
| Single instance | `interprocess`, and `libc` on Unix for the uid | Cross-platform local sockets, named from the data dir, so isolated instances never collide. |
| Tray (Linux) | `ksni` 0.3 | A StatusNotifierItem over D-Bus in pure Rust, with no libappindicator or GTK. Spawned with `assume_sni_available(true)`, so it follows a tray host that starts, stops or restarts later. Its `async-io` feature, not the default `tokio`, for the same reason as `zbus`. |
| Monitor list | `display-info` | iced exposes only the window's current monitor size; the `window.json` fits-on-screen check needs every monitor's bounds. |
| Tray (macOS, Windows) | `tray-icon` 0.25 | The Tauri team's tray crate, with `muda` menus (through its `tray_icon::menu` re-export, so the versions match). Default features off: they are Linux's (GTK, libappindicator). |
| Dock icon, drops on the menu bar icon (macOS) | `objc2` 0.6, `objc2-app-kit` 0.3, `objc2-foundation` 0.3 | Already in the tree through `tray-icon`. Switches the activation policy, so the app has a Dock icon only while its window is open. Registers the status item's window for file drops, with a delegate that reads the dropped file URLs (`tray-icon` has no drop support). |
| System light or dark mode (macOS, Windows) | `mundy` 0.2 (`color-scheme` only) | iced's own detection there reads the window's appearance, which iced pins to the app's theme, so it misses the system changing. `mundy` is what iced uses on Linux (already in the tree there), where its detection works. On macOS it must start on the main thread: `boot`. |
| Login item (Windows) | `windows-registry` 0.6 | The `Run` value for starting on login. From windows-rs, whose `windows-link`, `windows-result` and `windows-strings` are already in the tree. On Linux and macOS the entry is a small file the app writes itself (a `.desktop` file, a LaunchAgent plist), so no crate. |
| Windows exe resources | `winresource` (build dependency of `gui` on Windows only) | Embeds the icon Explorer and the taskbar show, and the name Task Manager lists, in `Ferry.exe`. |
| Packaging | Shell scripts in `packaging/`, NSIS on Windows | See "Packaging" below. |

## Desktop integration (plan step 10)

A throwaway spike (an iced `daemon` with `ksni`, `notify-rust`,
`interprocess` and `display-info`) ran on Linux on 2026-09-26: X11 under
Xvfb, Wayland under a headless labwc (wlroots), each on a private D-Bus
with a fake `StatusNotifierWatcher` and notification server (dbus-python),
and a GTK drag source driven with XTest. macOS and Windows weren't
available; their columns come from reading the winit 0.30.13,
`notify-rust` 4.18, `tray-icon` and `interprocess` 2.4 sources, for step
13 to confirm on those machines.

| | Linux X11 | Linux Wayland | macOS | Windows |
| --- | --- | --- | --- | --- |
| File hover and drop | Tested: `FileHovered` once per file, on entering; `FileDropped` per file; `FilesHoveredLeft` on leaving. Folders arrive too. | No drag and drop: winit has no `wl_data_device`, so a drag over the window is never accepted and nothing reaches the app. | Source: the same three events (`draggingEntered`, `performDragOperation`, `draggingExited`). | Source: the same (`IDropTarget`). |
| Cursor position during a drag | Tested: none. The drag source holds the pointer grab, so no `CursorMoved` arrives while hovering. With a GTK source, the ungrab gave one `CursorMoved` at the drop point just *before* `FileDropped`; that order is the source's doing. | n/a | Source: none; winit ignores `draggingUpdated`. | Source: none; winit drops `DragOver`'s point. |
| Tray | Tested (`ksni`): left click reaches `activate`, the menu has submenus and disabled items, and `handle.update` rewrites labels while running. Without a watcher `spawn` fails (`ServiceUnknown`) unless `assume_sni_available(true)`; with it, `watcher_offline`/`watcher_online` report the host going and coming, and the item re-registers on its own. | Same as X11 (D-Bus only). | Source (`tray-icon` + `muda`): must be created on the main thread after the event loop starts, so from the first `update` (iced runs `update` on that thread); its menu and icon events come from global channels, forwarded into a `Subscription`. | Same as macOS. |
| Close and reopen the window | Tested: `window::close` keeps an `iced::daemon` alive; `window::open` with `Position::Specific` and the saved size puts it back. `window::position`, `size` and `is_maximized` answer before closing. | Tested: works, but `window::position` is always `None` and `Position::Specific` is ignored: the compositor places windows. | Source: as X11. | Source: as X11. |
| Notifications | Tested: `notify-rust` shows it; a click on the body is the `default` action and reaches the app. Withdrawing needs the handle that `wait_for_action` consumed; by id it takes a replace-then-close that briefly shows an empty notification. | Same as X11 (D-Bus only). | Source: `NSUserNotificationCenter` (deprecated) reports the click only through a blocking wait per notification and can't withdraw; `UNUserNotificationCenter` (`preview-macos-un`) can, but needs the signed bundle. | Source: a body click is `Default` while the app runs; needs an AUMID (the installer's, step 15); no withdraw. |
| Single instance | Tested (`interprocess`, `GenericNamespaced`): an abstract socket, so no file: after `kill -9` the next launch listens at once; a second launch sends `show` and exits. | Same as X11. | Source: a socket file under `/tmp`, left behind by a crash; `try_overwrite(true)` replaces it after `connect` is refused. | Source: a named pipe; nothing stale. |
| Monitors | Tested: `window::monitor_size` is the current monitor's size only. `display-info` lists screens with origins, but picks X11 or Wayland from `WAYLAND_DISPLAY`, *then* `XDG_SESSION_TYPE`; its `is_primary` was false for the only screen. | Tested: `monitor_size` is `None` until the surface enters an output; `display-info` lists outputs. Positions don't matter here. | Source: `display-info` (AppKit). | Source: `display-info` (Win32). |

Decisions for steps 11 and 13:

- **Drops are routed, not hit-tested.** No platform reports where a drag
  is while it hovers, so there is no per-card highlight; the hover shows
  the window border and the pill. A drop goes to the route: on a device
  page, to that device (if a plugin's `drop_target` takes it); on a
  browse folder, into that folder; anywhere else, the "Send N files"
  chooser. The X11 drop position is unused: it depends on the source's
  event order and exists nowhere else, so the same drop would behave
  differently by platform.
- **Files dropped on the menu bar icon (macOS) are sent from a menu.**
  A menu pops up from the icon ("Send 2 files to:" and the devices that
  would take them) without opening the window, which would distract the
  user (`Tray::pop_up`; it is set as the status item's menu and clicked,
  as `tray-icon` opens its own). Where a tray can't pop one up, the
  window opens on the chooser. The status item's window is registered
  for file URLs and its delegate takes the drag (`desktop::tray`,
  `drops`); the icon highlights while files hover over it. Linux's
  StatusNotifierItem and Windows' notification area icon take no drops.
- **The shell filters out folders**, since winit hands them over like
  files ("Only files can be sent, not folders.").
- **Wayland: no drag and drop**, as the owner decided. Nothing to guard:
  the app never sees the drag.
- **The window closes to the tray rather than hiding.** `window::close`
  on close-to-tray, `window::open` with the saved placement to show it
  again; no GPU surface while in the tray.
- **Placement:** keep `{visible, maximized, bounds}`. Restore the position
  only where it can be set (X11, macOS, Windows) and only if it fits one
  of `display-info`'s monitors, otherwise centre; on Wayland restore size
  and maximized state and let the compositor place it. Read the position
  before closing (`window::position` answers `None` on Wayland, so keep
  the old one).
- **Tray availability** follows `watcher_online`/`watcher_offline`
  (`ksni`, `assume_sni_available(true)`); macOS and Windows always have
  one. Without a tray the window always shows at start, and closing it
  quits regardless of `closeToTray`, since only a second launch could
  show it again.
- **Notifications on Linux talk D-Bus directly** (`zbus`, on the
  daemon's runtime): `Notify` with a `default` action, `CloseNotification`
  to withdraw the pairing request, and the `ActionInvoked` and
  `NotificationClosed` signals from one subscription. **On macOS** the
  app uses `UNUserNotificationCenter` (`mac-usernotifications`, on the
  daemon's runtime): a click on the body shows the window, and the
  pairing request's notification is withdrawn, as on Linux. It needs the
  app's bundle; an ad-hoc signature is enough, but macOS refuses a bundle
  under `/tmp` ("Failed to find or validate client" in `usernoted`'s
  log), and without a bundle (`cargo run`) nothing shows. It asks the
  user's permission on first launch. Not `notify-rust`'s default
  `NSUserNotificationCenter` backend: unless it can pose as an installed
  app, it makes the whole process report Terminal's bundle id, and it
  hears a click only through a blocking wait per notification.
  `notify-rust` is a Windows-only dependency. **Clicking a notification
  shows the window on Linux, macOS and Windows;** withdrawing is Linux and
  macOS only.
- **Single instance:** `GenericNamespaced`, named
  `ferry-<uid>-<hash of the absolute data dir>` (short enough for
  macOS's 104-byte socket paths; the uid keeps users apart in Linux's
  shared abstract namespace). Connect first: a reply means another
  instance runs, so send `show` and exit; a refusal means listen with
  `try_overwrite(true)`.
- **Monitors:** `display-info` for the fits-on-screen check. Tests and
  Xvfb runs must set `XDG_SESSION_TYPE=x11` as well as unsetting
  `WAYLAND_DISPLAY`, or it looks for a Wayland compositor. Don't use its
  `is_primary`.

## Packaging (plan step 15)

- **Hand-written scripts, not `cargo-packager`.** The app is one binary
  per platform, so a package is a few files around it:
  `packaging/linux/build_deb.sh` (the `.deb`, with dependencies from
  `dpkg-shlibdeps` plus the libraries winit loads at runtime),
  `packaging/macos/build_app.sh` (a universal `.app` from `lipo`,
  `iconutil` and `codesign`, and the DMG from `hdiutil`) and
  `packaging/windows/installer.nsi` (NSIS). `cargo-packager` would be one
  more tool and config for the same result, and it can't derive the
  `.deb`'s dependencies or join two architectures into one binary.
- **The CLI is built on its own** (`cargo build -p ferry`): built
  together with `ferry-gui`, feature unification turns on `gui` and puts
  iced in it. `build_deb.sh` refuses a CLI with iced in it.
- **One app id, `dev.fanchao.Ferry`** (`ui::desktop::APP_ID`): the
  Linux window's app id and X11 class, the `.desktop` file, the icons and
  the notifications' `desktop-entry`; the macOS bundle id; the Windows
  notification id (AUMID), which the installer registers under
  `HKCU\Software\Classes\AppUserModelId`. The Flutter app had its own id,
  from before the app was named Ferry.
- **Names** (owner's decision): `/usr/bin/ferry-gui` (cargo's name for
  the app) and the CLI as `/usr/bin/ferry-cli` in the same `.deb`;
  `Ferry.app/Contents/MacOS/Ferry` and the CLI as
  `Ferry.app/Contents/MacOS/ferry-cli` (signed as nested code, before the
  bundle); `Ferry.exe` and the CLI next to it as `ferry-cli.exe`. The CLI
  was `ferry`, which on Windows had to live in a `cli\` folder since
  Windows ignores case; the installer removes that one on upgrade. The
  Windows installer is per user
  (`%LOCALAPPDATA%\Programs\Ferry`, no administrator rights), with a
  Start menu shortcut.
- **Icons** come from `assets/icon/*.svg` through
  `assets/generate_icons.sh`, which writes every platform's: the hicolor
  theme, the macOS iconset, the Windows `.ico`, the window icon and the
  tray icons.
- **Third-party notices** come from `cargo-about` (`about.toml`,
  `about.hbs`): nearly every dependency's license (MIT, BSD, Apache and
  the rest) asks for its notice to ship with the binary. The Build
  workflow writes `THIRD_PARTY_LICENSES.html`, and each package puts it
  where About's "Open source licenses" finds it from the running binary
  (`ui::desktop::licenses`): the bundle's `Resources`, next to
  `Ferry.exe`, and `/usr/share/doc/ferry`. `about.toml` lists the
  accepted licenses; a dependency under any other fails CI (the Licenses
  job) until someone accepts it. The Lucide font that `iced_fonts`
  embeds isn't in any crate's metadata, so its notice is written into
  `about.hbs` by hand.
- The `.deb` is built on Debian 12 and checked by
  `packaging/linux/check_deb.sh` on a clean Debian 12: it installs with
  its Depends only (no GPU driver; the app draws with tiny-skia), opens
  its window with its class and icon, and the CLI reaches its API.
- **Ferry is MIT** (`LICENSE`): the `.deb`'s `copyright`, the bundle's
  `Resources/LICENSE`, `LICENSE.txt` next to `Ferry.exe`, and the Arch
  package's `/usr/share/licenses/ferry-app-bin`.
- **Where users get it** (owner's decision: our own channels, not
  Homebrew's or Debian's official ones). After a full release the Build
  workflow publishes:
  - an **apt repository** on the website (`apt/`,
    `packaging/linux/apt_repo.sh`): one `stable` suite holding only the
    release's `.deb`s, checked by `check_apt_repo.sh` on Debian 12 and 13;
  - the **Homebrew cask** (`packaging/macos/ferry.rb.in`) in
    `simophin/homebrew-tap`, checked by installing it in the macOS job;
  - the **AUR package** `ferry-app-bin`, the release's PKGBUILD, checked by
    `makepkg` and `pacman -U` in the Arch job.
- **Signing.** Two keys, kept apart so a leak of one doesn't touch the
  other; the owner holds both offline, CI has what it needs as secrets
  (listed at the top of `build.yml`):
  - The **release key**, OpenPGP, ed25519
    (`packaging/release-key.asc`, fingerprint
    `ECD3B4C2913BF64B1664F19F074DA8D02070102E`), signs the apt repository
    and each release's `SHA256SUMS`. The primary key only certifies; CI
    has only its signing subkey, so a leaked subkey can be revoked and
    replaced without users changing the key they trust. It doesn't
    expire: apt refuses a repository whose key has.
  - A **self-signed code signing certificate** ("Ferry Code Signing",
    SHA-256 `7130536B…830B5B`, the full value in `build.yml`) signs the
    macOS app. Gatekeeper trusts only Apple's Developer ID, so it doesn't
    help there (the cask clears the quarantine); what it gives is one
    identity across releases, so the permissions users grant survive
    updates. A new certificate changes that identity once.
  - Windows stays unsigned: SmartScreen treats a self-signed Authenticode
    signature like none.
  - Apple's Developer ID and notarization ($99 a year) would make the DMG
    open without a prompt and the cask eligible for Homebrew's own
    repository; the owner chose not to, for now.

## Deliberate differences from the Flutter app

These differ on purpose (owner's decisions). Don't "fix" them back.

- **No external daemon mode.** Flutter could attach to a daemon through
  the API URL variable (now `FERRY_API_URL`); the app always embeds its
  daemon.
- **Configuration is flags and environment variables**, not compile-time
  defines (see Decision). The version in Settings and About is the git
  tag (`v1.2.0`): a release's from `FERRY_VERSION`, a dev build's from
  `git describe` (`v1.2.0-2-g9e6caee`), or `dev` without git.
- **No FFI.** `ffi/` existed only for Flutter and was deleted with it.
- **Retry restarts only what failed.** The "could not start" screen's
  Retry calls `RunningService::start` again; the tray keeps working.
- **No reconnecting banner.** There is no connection to lose in-process;
  a lagged receiver silently takes a fresh snapshot.
- **Start hidden works on every platform.** The window opens at start only
  if the saved placement says it was visible, or if there is no tray.
- **Single instance works everywhere**, through one local socket rather
  than GApplication or LaunchServices.
- **Wayland has no drag and drop** (winit has none there). Don't force X11
  or XWayland to get it back; *Send files* and *Upload files* do the same
  job.
- **Not sandboxed on macOS.** Flutter's sandbox caused the download-folder
  bookmark problem; the new app is a plain, ad-hoc-signed bundle unless
  the owner later wants the App Store.
- The window title is "Ferry", not the Flutter project's name.

## Consequences

- A feature is written once, in one language. Adding one means
  `src/plugins/<name>/` (`mod.rs`, `http.rs`) and a line in `builtin()`,
  its UI in `src/ui/features/<name>.rs` plus its lines in
  `features/mod.rs` (a `Feature` variant if it has messages, a line in
  each applicable `Features` function, and maybe a `Route` variant), and
  the CLI in `client.rs`/`cli.rs`.
- Anything the UI does must be a Rust function that `http.rs` also calls,
  so the UI and the CLI can't drift apart.
- A plain workspace build compiles the UI (feature unification); the CLI
  alone still builds without it.
- Linux builds need iced's system libraries (xkbcommon, Wayland); tests
  run headless.
- The app always embeds its daemon; to test against another instance,
  run a CLI peer.

# Handoff

For agents continuing Ferry: the ground rules, what "done" means, what is
still open, and how to verify in the real app.

The first UI milestone (items 1–11: unpairing, KDE Connect interop, tray,
transfers, settings, clipboard, end-to-end tests, ping, add by IP,
packaging, browsing a device's files) was built in Flutter and is finished;
[`archive/HANDOFF_UI_MILESTONE.md`](archive/HANDOFF_UI_MILESTONE.md) has
the detail behind each feature. The Flutter app was then replaced by a
native Rust and iced one ([`archive/PLAN_ICED_UI.md`](archive/PLAN_ICED_UI.md))
and deleted; its records are in
[`archive/flutter-adr/`](archive/flutter-adr/README.md).

## Read first

1. [`ARCHITECTURE.md`](ARCHITECTURE.md): the core and its plugins, the
   module map (§2), state machines, the full HTTP API, how the app embeds
   the daemon (§9), testing (§10), known gaps (§11), browsing a device's
   files (§12), the app's languages (§13).
2. [`adr/0001`](adr/0001-native-ui-in-iced.md): why the UI is Rust and
   iced in the daemon's process, its libraries, and its desktop
   integration per platform. It says which Flutter records still apply
   (0003 snapshot plus events, 0007 tray, 0008 browsing, 0009 window
   placement) and what differs from the Flutter app on purpose.

The app is `gui/` (`ferry-gui`). All UI code is in `src/ui/` (the shell,
and each feature's UI in `src/ui/features/`), behind the `gui` cargo
feature.

## Active implementation handoffs

- [Fixed plugin enum and native async dispatch](HANDOFF_STATIC_PLUGINS.md):
  planned follow-up to PR #76. The current PR still uses `Arc<dyn Plugin>`.
- [Archived plans and completed handoffs](archive/README.md): historical
  implementation notes; use this handoff and ARCHITECTURE for current rules.

## Ground rules (set by the project owner)

- **The UI is dumb.** It persists only the main window's placement
  (`window.json`); preferences are daemon settings, and its store is a
  cache of core snapshots. The one exception is starting on login: the
  system's login item is its state (ARCHITECTURE §7), so only the app can
  change it. The UI calls the same typed Rust functions of the core and
  plugins that `http.rs` calls, in-process (`adr/0001`), so the CLI can do
  anything the UI does. New behaviour goes into the plugin's or core's
  Rust API first, then `http.rs` and `client.rs`/the CLI, then
  `src/ui/features/<name>.rs`.
- **Every resource needs a snapshot and events.** The UI takes a
  snapshot, patches it from the event bus, and takes a fresh one after it
  lags (Flutter ADR 0003, carried over); the CLI does the same over
  `/events`. Without both, a client can't recover after a gap.
- **The daemon's data is in its store** (`ferry.db`, `src/store/`,
  [`adr/0002`](adr/0002-store-the-daemons-data-in-sqlite.md)). A small
  value is a `ConfigKey` declared by its owner and named `<owner>.<name>`
  (`core`, `ui`, or the plugin's id); plugins reach the store through
  `PluginContext::store()`. Lists get a table, which the core adds in a
  new migration (`src/store/migrations/`, never an edit to an old one).
  Nothing writes its own files in the data directory. A stored
  resource clients see still needs a snapshot and events: `Store::watch`
  only reaches code in the same process.
- **A feature is a plugin.** It lives in `src/plugins/<name>/`,
  implements `core::Plugin`, and is one line in `plugins::builtin()`
  (ARCHITECTURE §2). Its UI is `src/ui/features/<name>.rs` plus its lines
  in `src/ui/features/mod.rs`, the one place in the UI that lists
  features. The core doesn't name features and plugins don't import each
  other: shared code belongs in the core (or, for UI helpers, `src/ui/`).
- **Token auth is optional.** It is enforced only when the daemon was
  started with a token. The app's API is off until Settings → Command line
  access turns it on, and always has a token, kept in the store as
  `core.api` (or `--api-token` for one run); `ferry-cli run` defaults to
  none.
- **`cargo build -p ferry` has no iced in it.** UI code and its
  dependencies stay behind the `gui` feature.
- Use reputable dependencies and record new ones in `adr/0001`'s library
  table (or write a new ADR if the choice changes an existing decision).
- Update `ARCHITECTURE.md` when the API or module map changes.

Done means all of these pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets     # also builds and tests the UI
cargo build -p ferry                     # ferry-cli alone, without iced
cargo tree -p ferry -e normal --prefix none | grep -c '^iced'   # prints 0
git diff --check
```

Run `cargo test` under a private display and bus (CLAUDE.md). With `SNAPSHOT_DIR` set, the UI's
snapshot tests write light and dark PNGs of each page there, and PNGs in
the en-XA pseudo-locale (`*-en-XA-light-*.png`: accented, longer, in
brackets); look at them after a UI change. Plain English in an en-XA
image is a string that wasn't extracted, or test data; a missing closing
bracket is text cut off. `FERRY_LANG=en-XA` shows the real app in it,
as does `ferry-cli settings --language en-XA` on a running app (the
`language` setting switches it at run time; `system` goes back).
`SNAPSHOT_LANGUAGES=de,zh-CN` adds those translations
(`*-de-light-*.png`); German is the longest, Chinese needs CJK fonts.

## Strings and languages

Everything the app shows goes through `fl!` and the `.ftl` files
(ARCHITECTURE §13, [`PLAN_I18N.md`](archive/PLAN_I18N.md)); the CLI, the API and
logs stay English.

- **Adding a string.** Add the message to `i18n/en-US/ferry.ftl` in its
  feature's group, with a comment saying where it shows and what each
  argument is, and call `fl!("key", arg = value)`. Whole sentences only,
  names and numbers as arguments, counts through a plural selector (the
  rules are in the plan). Then add the key to every other
  `i18n/<lang>/ferry.ftl`: `ui::i18n::tests` fails while a language lacks
  it or has one en-US doesn't. If you can't translate it, copy the English
  there and say so in the PR, so a speaker can fix it; a removed key goes
  from every file. `fl!`'s check reads the files at build time, and
  editing only an `.ftl` doesn't trigger a rebuild: touch a `.rs` file.
- **Adding a language.** Copy `i18n/en-US/ferry.ftl` to
  `i18n/<tag>/ferry.ftl` (a BCP 47 tag, like `fr` or `pt-BR`) and
  translate every message, `package-*` included; nothing needs
  registering. Keep the header saying who wrote it and whether a native
  speaker has reviewed it. Plural selectors take the language's CLDR
  categories (`one`, `few`, `many`, `other`…; Chinese needs none).
  `packaging/i18n.sh` needs its NSIS name (`nsis_language`) and, for
  script variants, its `.lproj` name; the Windows build stops without the
  former. Add the tags systems report for it (macOS's carry a script, as
  in `zh-Hans-CN`) to `ui::i18n::tests::system_tags_reach_the_translations`,
  then look at the snapshots with
  `SNAPSHOT_LANGUAGES=<tag>`.

For anything involving windows, the tray, drops, dialogs or
notifications, also run the real app ("Verifying in the real app"):
snapshots render one frame and miss what only shows over time. Unit tests
with a fake daemon host missed two real bugs in the first milestone.

## Where things stand

Everything in the milestone works in the Linux app: checked live in the
Flutter app, and at each step of the iced rewrite against a CLI peer or
the fake phone. Against KDE Connect for Android (a Pixel 8a, from the CLI
daemon), pairing, unpairing, clipboard, file transfer both ways and
browsing the phone's files work. Nothing has been checked against KDE
Connect on a desktop yet.

Paired devices are listed even while offline (restored from the store).
The tray menu lists each connected paired device with its battery (send
files, ping, ring, send clipboard, browse files, show details); the
device list and details page show the battery too (`kdeconnect.battery`,
read-only). Releases (`.github/workflows/build.yml`) build a universal
macOS app in a DMG, a Windows installer, amd64 and arm64 Debian packages
holding the app and the CLI, and for tagged builds an Arch Linux
PKGBUILD; the scripts are in `packaging/`.

The daemon keeps its identity, paired devices and settings in one SQLite
database, `ferry.db`, with typed, watchable configs any plugin can declare
keys for ([`archive/PLAN_STORE.md`](archive/PLAN_STORE.md)). The old JSON
files (`identity.json`, `settings.json`, `trusted-devices/`) are ignored,
not migrated: an old data directory gets a new identity, and its devices
must be paired again.

## Open work

**The tray on macOS and Windows, and notifications on Windows** (the
plan's step 13b). Notifications work on Linux and macOS (ADR 0001,
"Desktop integration"). The tray works on macOS (checked by the owner) and
is untested on Windows. It is `tray-icon` + `muda` in
`src/ui/desktop/tray.rs` (`native`), created by `Tray::start`, which
`update` calls on the main thread once the event loop runs
(`Message::StartTray`, sent from boot); menu and click events go into the
`DesktopEvent` channel. macOS shows `assets/tray_icon_template.png` as a
template image and opens the menu on any click; Windows opens the window
on a left click and the menu on a right one. It type-checks for both
(below). Still to do:

- Run it on Windows: the icon shows, the menu's items and submenus work,
  it updates as devices come and go, and closing the window keeps the app
  in the tray. If `build()` fails, the app runs without a tray.
- On macOS the app lives in the menu bar: it has a Dock icon only while
  its window is open (`desktop::dock`, the activation policy; the bundle
  sets `LSUIElement`), because winit doesn't handle
  `applicationShouldHandleReopen:`, so a Dock icon with the window closed
  would do nothing. Check that the window comes to the front when opened
  from the tray, and that no Dock icon flashes when starting in the tray.
- `notify-rust` for Windows' `Notifier`: clicks work; nothing is
  withdrawn (ADR 0001, "Desktop integration").
- macOS notifications were checked (show, a click reaching
  `NotificationClicked`, withdrawal) from an ad-hoc signed bundle with the
  app's id, driving the real `Notifier` in an iced event loop, but not in
  the full app with a peer: loopback discovery doesn't work on macOS
  ("Traps"). They need the bundle, so `cargo run` shows none; run a bundle
  from outside `/tmp` (macOS refuses those), e.g. under `target/`. Check
  that a click brings the window (and its Dock icon) back while the app
  is menu-bar only.
- Files dropped on the menu bar icon (`drops` in `src/ui/desktop/tray.rs`)
  are only type-checked; the app's side (`DesktopEvent::TrayDropped`, the
  chooser) has unit tests. Check on a Mac that the icon highlights while
  files hover, the drop pops up the "Send N files to:" menu without
  opening the window, choosing a device sends them, a folder is refused
  with a notification, and clicking the icon still opens the tray's menu.
- macOS's menu-bar Quit and logout go through `terminate:`, which exits
  after winit's `exiting`, so the daemon's shutdown after `program.run()`
  likely doesn't run.
- Confirm single instance (the `/tmp` socket file on macOS, a named pipe
  on Windows) and placement with several monitors.
- Check what packaging couldn't: the DMG's app launches from Finder with
  its tray icon, and the installed Windows app's notifications carry its
  name and icon.

To type-check macOS or Windows code on Linux, stub out the C that `ring`
builds (it needs the platform's SDK) with a compiler and archiver that
emit empty files; nothing is linked, so clippy runs fine:

```sh
rustup target add aarch64-apple-darwin x86_64-pc-windows-msvc
# fakecc: find `-o <out>` and `--target=...`, then
#   exec clang $target -c -x c /dev/null -o "$out"
# fakear: find the archive (`$2`, or `-out:<path>`), write "!<arch>\n" to it
CC_aarch64_apple_darwin=fakecc AR_aarch64_apple_darwin=fakear \
  cargo clippy --target aarch64-apple-darwin -p ferry-gui -p ferry \
  --all-targets -- -D warnings
# Windows: the same with CC_/AR_x86_64_pc_windows_msvc and that target.
```

**The rest of the UI's open work:**

- Drag files out of the browser to the desktop (ADR 0008).
- A low-battery notification (`thresholdEvent`).
- Accessibility: check what iced 0.14 exposes to screen readers and
  record the gap against Flutter.

**Packaging.** Nothing is notarized. The macOS app is signed with a
self-signed certificate (ADR 0001, "Packaging"), and the Homebrew cask
clears the quarantine so Gatekeeper lets it open; the Windows installer is
unsigned. CI built the macOS and Windows apps, installed the Windows
installer and the cask, but neither has been used on a real desktop. Check
on a Mac that `brew install simophin/tap/ferry` opens without a prompt,
and that the Downloads folder permission survives a `brew upgrade`.

**Browsing a device's files** (ARCHITECTURE §12, ADR 0008):

- Not yet tried on a real phone: `rm`, the app's Browse files page, an
  older phone with an RSA key (only a unit test covers its host key), and
  how Android's `errorMessage` reads without "All files access".
- The 5-minute idle timeout has no test; it would need to be configurable.
- A recursive delete runs inside the 15-second request deadline, so a very
  large folder can stop partway; a background job with progress would fix
  that.
- `tests/ui_e2e.rs` covers browsing against the fake phone
  (`tests/support/fake_phone.rs`), not a real one.
- Not offered: dragging files out of the browser (ADR 0008), downloading
  whole folders, video thumbnails, and serving this machine's files (KDE
  Connect desktops don't either).

**Smaller follow-ups.**

- Ringing (`kdeconnect.findmyphone.request`: `ferry-cli ring`, the details
  page, the tray) was checked against the fake phone only. This machine
  doesn't ring when a peer asks it to.
- Notifications (`kdeconnect.notification`: `ferry-cli notifications`, the
  device's Notifications page, a desktop notification for each new one)
  were checked against the fake phone only. Worth checking on a phone:
  that it sends what it already shows once paired (the plugin asks when a
  device becomes paired and connected), how messaging apps' updates read
  (Android posts a new message as an update of the same notification,
  which alerts only when its text changes), that icons arrive, and a
  reply from the app. Not built: the `conversation` history Android sends
  for messaging apps, showing this machine's notifications on the phone,
  and per-app muting here (the phone picks which apps share).
- Calls (`kdeconnect.telephony`: `ferry-cli call`/`mute`, a notification
  with a Mute button while a phone rings, one per missed call) have only
  unit and API tests, and `--demo` (the made-up phone rings, then misses
  the call). Worth checking on a phone and on Linux and macOS: the
  notification's Mute button, its withdrawal on answer or hang-up, and
  that the phone's ringer really goes quiet. SMS isn't handled.
- Battery reports were checked against the fake phone only. A low-battery
  notification (`thresholdEvent: 1`) isn't shown, and this machine doesn't
  report its own battery.
- Mobile signal reports (`kdeconnect.connectivity_report`: a chip per
  SIM on the device list and details page, a label in the tray, a column
  in `ferry-cli devices`) were checked against the fake phone only. The
  tray shows only the network type ("LTE"), since its items have no
  icons for the bars.
- Saved device addresses (Add device's "Add by IP address" waits for the
  device, pairs, and keeps the address; the device page edits them) were
  checked between Ferry instances and in tests only. Worth checking against
  KDE Connect for Android over a real tailnet. The device page's gallery
  screenshots (`site/img/device-*`) predate its Addresses section.
- `plugins::clipboard::backend::system::tests::clearing_the_clipboard_is_not_reported`
  failed once in a full `cargo test --workspace` run and passed on every
  rerun and alone; it looks timing-sensitive under load.
- Snapshots carry no sequence number, so an event emitted just before a
  snapshot response can briefly be overwritten by older data (ADR 0003).
  If that shows up in practice, add the event bus sequence to list
  responses (e.g. a header) and drop older events.
- The reconnecting banner and the startup error screen have only been
  exercised in unit tests, not the real app.
- `ferry-cli send` prints the upload's response, taken once the last byte has
  been forwarded, so it ends on `transferring (N/N)` rather than
  `completed`. Waiting for the terminal state (or watching `/events`)
  would report the real outcome.
- A transfer the sender cancels shows on the receiver as `failed` with
  `connection_failed`, not `cancelled`: the receiver only sees the payload
  connection close early. KDE Connect's share protocol has no cancel
  notice either, so this probably stays; a UI could say "stopped by the
  sender" if it confuses people.

### Localization verification

The localization implementation plan is [archived](archive/PLAN_I18N.md).
Its remaining manual checks stay open here:

- Run the real app with `FERRY_LANG=en-XA` and check the pseudo-locale on
  screen, including clipping and untranslated strings.
- Check Chinese glyphs render without missing-glyph boxes in the real Linux
  app under Xvfb; prior verification only saw a macOS snapshot.
- Switch Language in the running app and verify its window and tray update
  without restarting. The loader is tested, but the archived notes did not
  verify this in the live app. Unset `FERRY_LANG` for this check so it does
  not override the setting.

Use the isolated data, ports, display and D-Bus recipe in CLAUDE.md.
German and Chinese translations also still need native-speaker review;
the archived plan records other platform verification limits.

## Traps

The UI's traps. Those about driving the app under Xvfb are in "Verifying
in the real app".

- **Two runtimes.** iced polls futures on its own executor. Anything that
  touches the daemon's tokio I/O (russh, payload sockets, file transfers)
  must run on the daemon's runtime (`UiContext::spawn`). Symptom: a panic
  "there is no reactor running", or a hang.
- **Don't block `update`.** Core calls that take locks are fine; I/O
  (SFTP, file reads, `RunningService::start`) goes through a `Task`.
- **Event ordering after subscribe-then-snapshot.** Events already in the
  snapshot are replayed after it. Device events carry the full device, so
  replaying one is harmless; transfers and pairings are why the store's
  guards exist, so don't drop them.
- **Actions outlive their page.** An event can remove the device while an
  action is in flight: messages carry ids, and handlers look the device up
  again rather than holding on to it.
- **Plugins don't import each other in UI code either.** A helper two
  plugins need (say, "upload files with a summary toast") belongs in
  `src/ui/`.
- **The tray is not a view.** It can't render an `Element`, which is why
  device actions are data. Don't add widget-returning tray APIs.
- **No shadows.** The app draws with tiny-skia, which repaints a shadow
  on every partial redraw, turning the widget black. Dialogs use a border
  instead; check anything new in the real app, since snapshots render one
  frame and don't show it.
- **macOS loopback.** `--discovery-loopback` can't find peers on macOS
  (no `127.255.255.255`). Use `--demo` there, and do peer tests on Linux.
- **Dialog titles are fixed text.** A name someone chose (a file, a
  device, a notification's sender) can be any length, so it goes in the
  body, which wraps anywhere (`Wrapping::WordOrGlyph`): "Unpair device?"
  with "Pixel 8a will need…" under it, not "Unpair Pixel 8a?".
- **iced is pinned** (`iced = "0.14"`, `iced_fonts = "0.3"`). Upgrading it
  is its own change, never mixed into a feature.

## Verifying in the real app

Isolate every run as [`../CLAUDE.md`](../CLAUDE.md) describes: fresh
temporary data and download dirs, a free port, loopback discovery, and a
private display and D-Bus session. `$udp` is a free UDP port (CLAUDE.md,
"Network"), shared by the app and its peers so the owner's own Ferry or
KDE Connect doesn't see them.

```sh
dir=$(mktemp -d -p "$scratchpad")
# peer
cargo run -- --api-port "$port" run --discovery-loopback --discovery-port "$udp" \
  --data-dir "$dir/peer" --download-dir "$dir/peer-downloads" \
  --device-name "CLI Peer"
# app (separate identity, loopback only), on a private display and bus
env -u WAYLAND_DISPLAY \
  dbus-run-session -- xvfb-run --auto-servernum \
  cargo run -p ferry-gui -- --discovery-loopback --discovery-port "$udp" \
    --data-dir "$dir/app" --download-dir "$dir/app-downloads" \
    --device-name "UI Desktop" --api-port "$app_port"
```

The app's daemon serves the API on `--api-port` (with `--api-token`, or
the token kept in its data dir's `ferry.db`, which `ferry-cli --data-dir
"$dir/app"` reads by itself), so the CLI can drive it: `pair` with the peer,
`send` to it, list its transfers. `--demo` fills the app with made-up
paired devices, for looking at the UI without a peer.

To try file browsing without a phone, run
`FERRY_DISCOVERY_PORT="$udp" cargo run --example fake_phone -- <DATA_DIR> <STORAGE_DIR> <DESKTOP_ID> [NAME]`
as the peer. It dials the desktop once, on the first announcement it
hears, so restart it (same data dir) after the desktop restarts. It trusts
the desktop's key only after a pairing request in the same run; after a
restart it lets the desktop in by password instead. To pair the app with
it without the UI, pair a CLI daemon started on the app's data dir first,
stop it, then start the app.

Without a display (e.g. in an agent sandbox), run the built bundle under
`Xvfb`, take screenshots with `import -display :NN -window root out.png`,
and click with XTest (`libXtst` through Python `ctypes`). Don't open
windows on the user's own session, and don't pair with or send to real
devices on their network without asking.

The README and website share eight pairs of screenshots,
`site/img/<page>-<light|dark>.webp`, rendered from the real iced views with
bundled Inter at 2x by the snapshot fixtures. The export keeps
images 880 px wide (downsampling the wide file preview). Both galleries
use `<picture>` sources with `media="(prefers-color-scheme: dark)"` and a
light `<img>` fallback, so the browser chooses the theme without JavaScript.
The website also self-hosts Inter in `site/fonts/`.

To retake every gallery image after a UI change (Pillow is needed for
WebP export):

```sh
shots=$(mktemp -d /tmp/ferry-screenshots.XXXXXX)
env -u WAYLAND_DISPLAY GDK_BACKEND=x11 SNAPSHOT_DIR="$shots" \
  dbus-run-session -- xvfb-run --auto-servernum \
  cargo test -p ferry --features gui --lib snapshot
python3 site/screenshots.py "$shots"
```

Inspect the light and dark outputs before committing. The fixtures use
made-up devices, transfers and files; `snapshot_pairing_request` is the
clean incoming prompt, while the other incoming fixture exercises errors.
`snapshot_documentation_gallery` captures the shell with its real built-in
features, avoiding the synthetic Wave/Hug extension-point test fixture.
`files-folder-narrow` shows a file's actions and `files-preview` previews
a generated example image. Other snapshots (including pseudo-locales)
stay in the temporary directory; remove that directory after inspection.
For a live capture, `WINIT_X11_SCALE_FACTOR=2` renders at 2x and
`ICED_THEME=Light` or `ICED_THEME=Dark` overrides the system appearance.

- Launch the app under `env -u WAYLAND_DISPLAY DISPLAY=:NN
  GDK_BACKEND=x11 dbus-run-session -- ...`, with
  `env` *outside* `dbus-run-session`. Services the private bus starts (the
  file chooser portal, notifications) inherit the bus daemon's
  environment; with `env` inside, they get the owner's
  `DISPLAY`/`WAYLAND_DISPLAY` and open on the owner's desktop.
- There is no window manager, so a click doesn't focus the app's window:
  call `XSetInputFocus` on it (through `libX11` with ctypes) before
  sending keys.
- Unsetting `WAYLAND_DISPLAY` isn't enough to keep a GTK or Wayland helper
  off the owner's desktop: they fall back to `$XDG_RUNTIME_DIR/wayland-0`.
  Set `GDK_BACKEND=x11` (and `XDG_SESSION_TYPE=x11` for `display-info`);
  a headless compositor needs its own short `XDG_RUNTIME_DIR` (socket
  paths are limited to 108 bytes, so not under the scratchpad).
- The "Send files" picker opens as a GTK dialog on the virtual display
  (through `xdg-desktop-portal`, which D-Bus starts on your private bus).
  Without a window manager it can be bigger than the screen and unfocused:
  move it with `XMoveResizeWindow` and focus it with `XSetInputFocus`.
  Then press Ctrl+L, type the absolute path and press Return, all as XTest
  key events.
- The dialog starts in "Recent", listing the user's real recent files.
  Pick files by typed path, and don't browse or screenshot more of it than
  you need.
- Loopback transfers in a debug build run at tens of MB/s, so use a 1–2 GB
  file (from `/dev/zero`, in the scratchpad) to catch progress mid-flight
  or cancel it.
- "Open file" and "Open folder" launch the desktop's real default app
  (e.g. Thunar) on the virtual display, which may start helpers
  (`xfconfd`, `tumblerd`) that outlive it. Kill them afterwards, checking
  `/proc/<pid>/environ` first that they belong to the private bus.
- The portal starts `xdg-document-portal`, which mounts its FUSE file
  system at the owner's `$XDG_RUNTIME_DIR/doc` if nothing is mounted
  there. Stop every process on your bus when done, and check the mount is
  as you found it.
- Quit the app with Ctrl+Q in its focused window, or through the tray's
  Quit, which works without a tray host: read `DBUS_SESSION_BUS_ADDRESS`
  from `/proc/<app pid>/environ`, get the menu with `gdbus call --session
  --dest org.kde.StatusNotifierItem-<pid>-<n> --object-path /MenuBar
  --method com.canonical.dbusmenu.GetLayout -- 0 -1 '["label"]'` (find the
  name with `ListNames`), and send `com.canonical.dbusmenu.Event --
  <Quit's id> clicked '<"">' 0`. Closing the window only hides it.
- Don't clean up with `pkill -f <pattern>`: the pattern also matches the
  shell running the command and kills it. Kill by PID. `setsid cmd &`
  forks, so its `$!` is a wrapper that has already exited: record the PID
  from `pgrep -f` with your run directory in the pattern.

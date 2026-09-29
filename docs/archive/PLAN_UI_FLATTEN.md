# Plan: move the UI's feature code into `src/ui/` and drop the plugin seam

> **Archived.** This plan is finished and kept for the history behind each
> PR. What is still in force lives elsewhere: the decision and why in
> [`adr/0001`](../adr/0001-native-ui-in-iced.md); the shape of the code in
> [`ARCHITECTURE.md`](../ARCHITECTURE.md) §2; the ground rules in
> [`HANDOFF.md`](../HANDOFF.md). Links below are as they were before the
> move.

For agents doing this work. Each agent starts clean, so this file carries
the context: why, what the code looks like now, the target, and what
"done" means for each PR. Read [`HANDOFF.md`](HANDOFF.md) first for the
ground rules, the done-means checks and how to run things in isolation
(CLAUDE.md). **Where this plan and the docs disagree about where UI code
lives, this plan wins**: PR 3 brings the docs in line.

Status (2026-09-26): done. PR 1 (#28), PR 2 (#29), PR 3 (branch
`ui-flatten-docs`).

| PR | What | Needs |
| --- | --- | --- |
| 1 | Move each feature's `ui.rs` into `src/ui/features/` **and** replace the erased `UiPlugin` seam with concrete messages and routes. **Done** | — |
| 2 | Split the two big files: `ui/features/browse.rs` and `ui/mod.rs`. **Done** | PR 1 merged |
| 3 | Docs: ADR 0001, `ARCHITECTURE.md`, `HANDOFF.md`, `README.md`. **Done** | PR 1 merged; can run beside PR 2 |

Each PR is its own branch off `main` and must pass every done-means check
on its own. None of them changes what the app does or how it looks.

## Why

The iced rewrite (PR #25) put each feature's UI half next to its core
plugin (`src/plugins/<name>/ui.rs`) and fixed a rule: **the UI core never
names a feature.** Keeping that rule costs a lot of machinery for six
features that are all compiled in, and the owner finds the result hard to
read:

- `src/ui/plugin.rs` (768 lines): a `UiPlugin` trait with 10 hooks, an
  `ErasedUiPlugin` trait that re-declares every hook with a blanket impl,
  just to erase the message type.
- `PluginMessage`: `Arc<dyn Any>` plus a `&'static str` id, routed by
  string and downcast at runtime. A misrouted message panics instead of
  failing to compile.
- `Command<M>` / `Outcome<M>` / `ShellRequest<M>`, each with a `map`: a
  second copy of iced's `Task<Message>`, needed only because a plugin
  can't name the app's `Message`.
- `Route::Plugin { plugin: &'static str, device, page: String }`: browse
  encodes its folder in a string (`files:{path}`) and parses it back.
- `plugins::builtin_with_ui()`, a second list of the plugins that must stay
  in the same order as `builtin()`, with a test to check it.

Runtime or third-party plugins are an explicit non-goal
(`docs/archive/feature-modules.md` §1), so this abstraction buys nothing.

**The decision:** all UI code lives in `src/ui/` (a module, not a crate:
a crate would force everything the UI touches in `core`/`plugins` to
become public API). Feature UIs are plain modules under
`src/ui/features/` that the shell calls by name. There's one app `Message`
enum, typed routes, and plain `Task<Message>`.

**What stays as it is:** the core `Plugin` trait, `src/plugins/` (minus
`ui.rs`), the HTTP API, the CLI, the wire protocol. The core still never
names a feature; only the UI does. `DeviceAction` stays as data (one list
drives both the device page's buttons and the tray menu, so they can't
drift apart). Plugins still don't import each other.

**The trade-off, accepted by the owner:** adding a feature now touches a
few lines in `src/ui/features/mod.rs` (a message variant and one line in
each dispatch function that applies) and maybe a `Route` variant. That's
fine: they're short, and the compiler flags a missed `match` arm.

## What's there now (at `d49359d`)

Feature UIs and the hooks each one implements:

| File | Lines | State | Hooks it fills |
| --- | --- | --- | --- |
| `src/plugins/ping/ui.rs` | 172 | none | `device_actions`, `on_event` (notify on a received ping), `update` |
| `src/plugins/findmyphone/ui.rs` | 136 | none | `device_actions`, `update` |
| `src/plugins/battery/ui.rs` | 184 | none | `device_status`, `demo_packets` (no messages: `enum Message {}`) |
| `src/plugins/clipboard/ui.rs` | 285 | `Arc<ClipboardPlugin>` | `device_actions`, `view_settings`, `update` |
| `src/plugins/share/ui.rs` | 322 | none | `device_actions`, `drop_target`, `update` |
| `src/plugins/browse/ui.rs` | 2384 (tests from line 1432) | lots: open place, listings, sort, preview, menu… | `device_actions`, `view_page`, `drop_target`, `on_route`, `on_event`, `subscription`, `update` |

The seam and its users: `src/ui/plugin.rs` (the traits, `PluginMessage`,
`Command`, `Outcome`, `ShellRequest`, `DeviceAction`, `DeviceStatus`,
`DropTarget`, `Callback`, `Validator`, `Icon`, `UiContext`,
`on_runtime`), `src/ui/mod.rs` (`Running.plugins: Vec<Box<dyn
ErasedUiPlugin>>`, `Message::{Plugin, TrayPlugin, Shell}`,
`plugin_update`, `handle`, `shell_task`, `drop_target`, `plugin_page`,
`subscription`), `src/ui/route.rs`, `src/ui/demo.rs`,
`src/ui/background.rs` (tray menu), `src/ui/desktop/tray.rs`
(`TrayCommand::Action(PluginMessage)`), `src/ui/overlay/dialog.rs`
(`Callback`, `Validator`), `src/ui/pages/{devices,device,settings}.rs`
(take `&[Box<dyn ErasedUiPlugin>]`), `src/plugins/mod.rs`
(`Builtin`, `builtin_with_ui`), `gui/src/main.rs` and `tests/ui_e2e.rs`
(both call `builtin_with_ui` and build `ui::Started { service, plugins }`).

Behaviour the seam carries that **must survive exactly**:

- **Origin.** A feature message comes from the window or the tray
  (`Origin`, `src/ui/mod.rs`). `App::handle` treats a tray-originated
  request differently: toasts are dropped; `Report` shows only failures,
  as a desktop notification titled with the failure; `Navigate`, `Confirm`
  and `Prompt` also show the window; files picked after a tray action go
  back as tray-originated, so a failure to send them is reported the
  tray's way. Follow-up messages a feature sends itself keep the origin
  of the message that caused them (`shell_task`).
- **Order.** Actions, status chips and settings sections appear in
  `builtin()` order: ping, findmyphone, battery, clipboard, share, browse.
- **Drops.** A drop goes to the first feature that takes it, starting
  with the one that owns the current route (on a browse folder, browse
  uploads into it; elsewhere on a device page, share sends it).
- **Route changes.** Every route change reaches browse (`on_route`), so it
  loads a folder when its page opens and drops its listings when the page
  closes. Every core event reaches ping and browse (`on_event`), after the
  store has applied it.
- **Demo.** `--demo` ticks ask every feature for packets (only battery
  makes any).

## Target shape (after PR 1)

```
src/ui/
  mod.rs              App, Message, Origin, Running, update/view/subscription (shell)
  context.rs          UiContext and on_runtime (moved out of plugin.rs)
  shell.rs            what features ask of the shell, as functions returning Task<Message>
  features/
    mod.rs            Features, Feature, and every dispatch fn: the one place that lists features
    ping.rs findmyphone.rs battery.rs clipboard.rs share.rs browse.rs
  route.rs pages/ overlay/ desktop/ widgets.rs store.rs sync.rs demo.rs ...
```

`src/ui/plugin.rs` is gone. So are `src/plugins/*/ui.rs`, `pub mod ui`
in each plugin, and every `#[cfg(feature = "gui")]` in `src/plugins/`.

A sketch; the names are suggestions, but the shape is the point: no
traits, no generics over a feature's message, no `Any`.

```rust
// src/ui/mod.rs
pub(crate) enum Message {
    // ...the shell's existing variants...
    /// A feature's message, and where the action that caused it came from.
    Feature(Feature, Origin),
    // What features ask of the shell (was ShellRequest), as plain messages:
    Toast { text: String, action: Option<(String, Route)> },
    Report { text: String, failure: Option<String>, origin: Origin },
    Notify { title: String, body: String },
    Navigate(Route, Origin),
    Confirm { title: String, body: String, confirm_label: String, then: Box<Message> },
    Prompt { /* as ShellRequest::Prompt */ then: Callback<String>, origin: Origin },
    PickFiles { title: String, then: Callback<Vec<PathBuf>>, origin: Origin },
}

// src/ui/features/mod.rs
#[derive(Debug, Clone)]
pub enum Feature {
    Ping(ping::Message),
    FindMyPhone(findmyphone::Message),
    Clipboard(clipboard::Message),
    Share(share::Message),
    Browse(browse::Message),
}   // battery has no messages, so no variant

/// The features that keep state. The stateless ones are free functions.
pub struct Features {
    pub clipboard: clipboard::ClipboardUi,
    pub browse: browse::BrowseUi,
}

impl Features {
    pub fn update(&mut self, ctx: &UiContext, feature: Feature, origin: Origin) -> Task<Message>;
    pub fn device_actions(&self, device: &DeviceSnapshot) -> Vec<DeviceAction>; // builtin() order
    pub fn device_statuses(&self, device: &DeviceSnapshot) -> Vec<DeviceStatus>;
    pub fn on_event(&mut self, ctx: &UiContext, event: &CoreEvent) -> Task<Message>;
    pub fn on_route(&mut self, ctx: &UiContext, route: &Route) -> Task<Message>;
    pub fn drop_target(&self, device: &DeviceSnapshot, route: &Route) -> Option<DropTarget>;
    pub fn settings_sections<'a>(&'a self, settings: &'a SettingsSnapshot) -> Vec<Element<'a, Message>>;
    pub fn subscription(&self) -> Subscription<Message>;
    pub fn demo_packets(&self, device: &DeviceSnapshot, tick: u64) -> Vec<Packet>;
}
```

Notes on the pieces:

- **`DeviceAction`** drops its type parameter: `message: Feature`. The
  device page wraps it as `Message::Feature(f, Origin::Window)`, the tray
  as `Origin::Tray` (`TrayCommand::Action(Feature)`). Same for
  `DropTarget::on_drop: Arc<dyn Fn(Vec<PathBuf>) -> Feature + Send + Sync>`.
- **Origin threading.** A feature's `update` takes `origin` and passes it
  to the `shell::*` helpers (`shell::done(origin, text)`,
  `shell::failed(origin, title, text)`, `shell::navigate(origin, route)`,
  `shell::pick_files(origin, title, then)`, …) and to the messages it
  sends itself (`Message::Feature(Feature::Browse(m), origin)`). Messages
  from `on_event`, `on_route` and subscriptions are `Origin::Window`, as
  today. `App::handle`'s per-origin rules move to the handlers of the new
  shell `Message` variants, unchanged.
- **`Callback<A>`** becomes non-generic: `Arc<dyn Fn(A) -> Message + Send +
  Sync>`. `Validator` and the dialog's generic `Callback<A, M>` move to
  `overlay/dialog.rs`, which stays generic over its message as a widget.
- **Routes.** `Route::Plugin { .. }` becomes
  `Route::Browse { device: String, folder: Option<String> }` (the storage
  root is `None`). `parent()` is `Device`, and `device()` includes it.
  Browse's `route()`, `Place::of` and the `PAGE`/`files:` string parsing go.
  `App::plugin_page` becomes a `Route::Browse` arm in `App::page`,
  including the "device is gone" case it handles now.
- **Pages** (`devices`, `device`, `settings`) take `&Features`, or the
  already-computed `Vec<DeviceAction>`/`Vec<DeviceStatus>`, whichever
  reads better, instead of `&[Box<dyn ErasedUiPlugin>]`.
- **`UiContext`** keeps what it has (`core`, `store`, `spawn`, `device`,
  `transfers`, `window_focused`, `plugin_context`) but `spawn` returns
  `Task<T>` (or takes a `then` that makes a `Message`) instead of
  `Command`. Keep its `spawned_work_runs_on_the_daemon_runtime` test.
- **Construction.** Replace `plugins::Builtin`/`builtin_with_ui` with a
  non-`gui` function that builds the core list once and also returns the
  two instances the UI needs, e.g.
  `plugins::builtin_parts(clipboard) -> Parts { core, clipboard:
  Arc<ClipboardPlugin>, browse: Arc<BrowsePlugin> }`, with `builtin()`
  returning `builtin_parts(..).core`. Now there's one list, so delete the
  order test. `ui::Started` carries `service` plus those two `Arc`s, and
  the UI builds `Features` from them. Update `gui/src/main.rs` and
  `tests/ui_e2e.rs` to match.
- **Visibility.** Moved code imported private items through `super::`.
  Most are already `pub`. `plugins::browse::files` is a private module
  (`join_remote_path`, `split_remote_path`): make it `pub(crate)` or
  re-export the two functions. Raise anything else to `pub(crate)`, not
  `pub`, unless it already is.

## PR 1: flatten the UI

Branch suggestion `ui-flatten`. One PR, because a pure file move that keeps
the trait would be churn that the second half immediately rewrites.

**Done (2026-09-26)**, in two commits: the `git mv` (still on the trait,
with the construction change), then the rewrite. Where it differs from
this plan:

- **Callbacks make a `Feature`, not a `Message`.** `features::Callback<A>`
  (drops, picked files, a prompt's text) and `Message::Confirm`'s `then`
  are the feature's message; the shell adds the origin: `Window` for what
  a dialog sends, the request's own origin for picked files. So the
  per-origin rules stay in the shell's handlers, as before.
- **Shell messages.** `Toast` carries an origin too (the tray drops
  toasts). `Navigate(Route, Origin)` replaces the shell's old
  `Navigate(Route)`: from the tray it also shows the window. `Prompt` and
  `PickFiles` hold a `shell::Prompt` / `shell::PickFiles` struct (they
  hold closures, so they need a hand-written `Debug`). `ShowWindow` went:
  no feature asked for it. `PickFiles` lost its `confirm_label`, which the
  shell never used (rfd can't relabel the button).
- **Pages** take closures, not `&Features`, so their tests keep their
  stand-in chips and actions and the snapshots stay the same: `devices`
  takes `statuses`, `device` a `DeviceFeatures { statuses, actions }`,
  `settings` a `sections` closure.
- **`UiContext::spawn`** stays generic (`-> Task<M>`); features pass a
  `then` that makes the app's `Message`.
- **Construction.** `plugins::builtin_parts(clipboard) -> Parts { core,
  clipboard, browse }`; `ui::Started { service, clipboard, browse }`.
- **Visibility.** `Message`, `Origin`, `KeyCommand` and `Handoff` are
  `pub(crate)` (a `pub(crate)` `Message` can't hold private types);
  `plugins::browse::files` is `pub(crate)`.
- **`Icon`** moved to `widgets`, `Validator` and the dialog's
  `Callback<A, M>` to `overlay/dialog.rs`.

Tests: the two erasure tests and the plugin order test are gone with
what they tested. The shell's tests that used the fake `opener` plugin
now drive shell messages or real features: toast and navigate (a `Toast`
message, then browse's action), confirm (a `Confirm` whose `then` pings
a connected peer), the forgotten device's page (`Route::Browse`), and the
tray action that opens a page (browse's tray item, plus a tray `Toast`
that must not show). The tray-label test uses battery's real chip (a
battery packet), and the page tests use closures in place of the fake
plugins. Snapshots are pixel-identical to `main` (66 files).

Suggested order inside the PR (commit as you go; each commit should
build):

1. `git mv src/plugins/<name>/ui.rs src/ui/features/<name>.rs` for all six
   and fix the imports (`super::X` becomes `crate::plugins::<name>::X`),
   still on the trait. Register them in `ui/features/mod.rs`; move the
   list out of `builtin_with_ui` into the UI. Build and test. Using
   `git mv` keeps `git log --follow` working.
2. Add `Feature`, `Features`, the shell `Message` variants and `shell.rs`.
   Port one stateless feature (ping) end to end, then the rest, browse
   last.
3. Replace `Route::Plugin` with `Route::Browse`.
4. Delete `ui/plugin.rs` and `builtin_with_ui`; move `UiContext` to
   `context.rs`.

Tests:

- Each feature's unit tests move with it and keep asserting the same
  things, through the new types (for example a ping action's message is
  `Feature::Ping(..)`; its update yields a `Message::Report`).
- `src/ui/plugin.rs`'s erasure tests (`a_message_goes_round_the_erased_plugin_and_back`,
  `a_message_for_another_plugin_is_a_bug`) test the deleted machinery;
  delete them.
- `src/ui/mod.rs`'s tests use a test-only fake plugin (`opener()`, id
  `"opener"`) to exercise the shell: toast plus navigate, confirm sends
  only on confirm, tray-origin reporting, picked files from the tray, a
  plugin page for a forgotten device, drop targeting, and the tray labels
  test that fakes a `device_status`. The fake can't exist without the
  trait. **Keep every behaviour covered.** Drive the shell messages
  directly (`Message::Report { origin: Origin::Tray, .. }`, …) or go
  through a real feature (ping for reports, browse for navigation and the
  forgotten-device page, share for drops, battery for status), whichever
  gives the clearer test. List the tests you dropped or merged, and why,
  in the PR description.
- `tests/ui_e2e.rs` must pass unchanged apart from construction.

Done means:

- The done-means checks in `HANDOFF.md`, including
  `cargo build -p myconnect` without iced and the `cargo tree` check.
- `grep -rn "UiPlugin\|PluginMessage\|ShellRequest\|builtin_with_ui\|Route::Plugin\|dyn Any" src gui tests`
  finds nothing (docs may still mention them until PR 3).
- **Pixel-identical snapshots.** Run the UI tests with `SNAPSHOT_DIR` set
  on `main` and on the branch, into two directories, and compare them
  (`diff -r` or `cmp` per file). Any difference is a behaviour change to
  explain or fix. Same Xvfb/`ICED_BACKEND=tiny-skia` setup for both runs
  (CLAUDE.md).
- A run of the real app (CLAUDE.md's isolated recipe, `--demo` plus a
  loopback CLI peer or the fake phone) that clicks through: a device's
  actions from the page **and from the tray** (ping, ring, send
  clipboard, send files, browse), a failed tray action producing a
  notification, a drop on a device page and on a browse folder, browse
  navigation and Back, the clipboard settings switch. Say in the PR
  what you ran and what you couldn't (for example, no Xvfb on macOS).
- The PR description gives the line count before and after for `src/ui/`
  plus the old `ui.rs` files.

Traps:

- Don't change any user-visible wording, layout or ordering. This is a
  refactor.
- iced's `Subscription::map` takes only non-capturing closures (the
  reason for `.with(id)` in the old code). `Message::Feature(Feature::Browse(m), Origin::Window)`
  needs no capture; write it as a `fn`.
- `UiContext::spawn` must keep running futures on the daemon's tokio
  runtime (`on_runtime`), not iced's executor, which has no reactor.
- Browse is the bulk of the work (22 `ShellRequest` uses). Leave splitting
  its file to PR 2; keeping it one file here keeps the diff reviewable.

## PR 2: split the big files

Branch suggestion `ui-split`. After PR 1 is merged. Pure moves: no
behaviour change, no new abstractions (no new traits; `impl App` blocks
spread across files are fine).

- **`src/ui/features/browse.rs`** (~2.4k lines, about 950 of them tests)
  becomes `src/ui/features/browse/`. Split it by concern: for example
  `mod.rs` (state, `update`, dispatch entry points), `view.rs` (the page,
  rows, toolbar), `preview.rs` (image preview and its decoding),
  `listing.rs` (fetching, sorting, filtering), with tests next to the code
  they test. Read the file first and follow its natural seams rather than
  this list.
- **`src/ui/mod.rs`** (~3.7k lines; ~1.7k code, tests from about line
  1733) splits by concern. Candidates: shell requests and dialogs, window
  lifecycle (show, hide, close to tray, quit, placement; see what already
  lives in `background.rs`), drops and the recipient chooser, startup and
  retry. Move each group's tests with it. `mod.rs` should end up as the
  `App`/`Message` definitions, `update`'s top-level dispatch, `view` and
  `subscription`.

Done means: the HANDOFF checks, pixel-identical snapshots against `main`
(as in PR 1), and `git diff -M --stat` showing mostly moves.

**Done (2026-09-26)**, in two commits, one per file. Every test moved
with its code; the same 323 lib tests pass, and the 66 snapshots are
byte-identical to `main`. Apart from imports and module docs, the only
edits are visibility (`pub(super)` on what a parent now calls in a
child) and paths that lost a prefix (`shell::Prompt` inside `shell.rs`).
The split:

- **`ui/features/browse/`** (2392 lines before): `mod.rs` (1276: state,
  messages, `update`, routing, drops, and the `Browser` test harness),
  `view.rs` (the page, breadcrumbs, listing, rows, sorting), `preview.rs`
  (the image preview and decoding), `files.rs` (the `Files` trait over
  `BrowsePlugin`, and the fake phone the tests use), `describe.rs` (error
  wording). No `listing.rs`: fetching is `BrowseUi`'s state and stays
  with it; sorting went to `view.rs`, where it is used.
- **`ui/mod.rs`** (3614 lines before, 829 after): `launch.rs` (the public
  entry points `run`, `program`, `UiOptions`, `Started`, `Service`,
  `Desktop`, re-exported from `ui`; booting and Retry), `shell.rs` gained
  the `App` side of what features ask (toasts, reports, dialogs, prompts,
  pickers, timers), `background.rs` gained the window's life (show,
  close to the tray, quit, placement) with the tray and notifications,
  `drops.rs` (dropped files and the chooser), `actions.rs` (what the
  shell's own pages ask of the core: scanning, add by address, pairing,
  unpairing, transfers, received files, settings). The shared test
  harness (fakes, app builders, `settle`, `click`) is `ui/tests.rs`,
  with `mod.rs`'s own tests. `react` stays whole in `mod.rs`: it is the
  dispatch.

Checked in the real app (`--demo`, loopback, Xvfb, private bus): the
device list, a device page, the Unpair confirmation and Cancel, the tray
menu over dbusmenu (Show details, Browse files, which open the window on
their page), Back, and Quit from the tray saving `window.json`.

## PR 3: docs

Branch suggestion `ui-flatten-docs`. After PR 1 is merged; it can run
beside PR 2. Whichever of the two lands second fixes the module map row
for `ui` in `ARCHITECTURE.md` §2 to match the final file list.

- **`docs/adr/0001-native-ui-in-iced.md`**: replace the "UI halves live in
  the feature modules" decision with the new one (all UI code in
  `src/ui/`, feature UIs in `src/ui/features/`, named by the shell) and
  record why: the reasons in this plan's "Why". Update the "A `gui` cargo
  feature" bullet (it now covers `src/ui/` only) and the lines near 231
  on what adding a feature takes. Keep the ADR's history honest: say the
  first shape was tried and replaced, and when.
- **`docs/ARCHITECTURE.md`**: §2's dependency lines (46–48), the
  `plugins`, `ui` and `myconnect-gui` rows of the module map (76–82), the
  paragraph on the UI not naming features (around 58–61), "adding a
  feature" (around 146–148) and §9's `start_with` example (around 397).
- **`docs/HANDOFF.md`**: the paragraph on where the app lives (around
  line 31), the "UI is dumb" rule (which ends "then to `ui.rs`"), and the
  "A feature is a plugin" rule. A feature is still a core plugin; its UI
  is `src/ui/features/<name>.rs` plus its lines in `features/mod.rs`.
  Drop the pointer to this plan and move this file to `docs/archive/`
  with an "Archived" note at the top, as `archive/PLAN_ICED_UI.md` has.
- **`README.md`** line ~133.
- `docs/archive/feature-modules.md` is history: leave it, apart from
  one line at the top saying the UI half of it was replaced, pointing to
  the ADR.

Done means: `grep -rn "ui\.rs\|UiPlugin\|builtin_with_ui\|never names a feature" docs README.md CLAUDE.md`
finds only `docs/archive/` and the research doc's history, and
`git diff --check` is clean.

**Done (2026-09-26)**, after PR 2, so it also fixed §2's `ui` row to the
split file list. Where it differs from this plan:

- **ARCHITECTURE §2** keeps a dependency line for `ui::features` (it
  imports `plugins/*`' typed APIs) instead of the one for
  `plugins/*/ui.rs`, and the paragraph on the UI now says `ui::features`
  is the one place that names features; the shell imports `plugins` only
  to carry the clipboard and browse instances. The `ui` row describes the
  shell files by concern (`launch`, `shell`, `background`, `drops`,
  `actions`, `tests`) and the feature modules.
- **ADR 0001** replaces the decision in place, with a paragraph on the
  first shape (PR #25) and why it went (PRs #28, #29), rather than a new
  ADR: the rest of the record (iced, in-process core, libraries, desktop
  integration) is unchanged.
- Other mentions of `builtin_with_ui` (ARCHITECTURE's `myconnect-gui` row
  and `plugins` row) became `builtin_parts`.

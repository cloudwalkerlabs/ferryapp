# Plan: localising the app

> Archived on 2026-09-29: implementation is complete. Outstanding manual
> verification is tracked in [the active handoff](../HANDOFF.md#localization-verification).
> The phase instructions and notes below are historical. Current rules
> live in [HANDOFF.md](../HANDOFF.md) and [ARCHITECTURE §13](../ARCHITECTURE.md#13-the-apps-languages).

How the desktop app (`ferry-gui`, `src/ui/`) gets translated. Each phase
is done by one agent: read this plan and `HANDOFF.md`, do the phase,
pass the "done means" checks in `HANDOFF.md` (run under a private display
and bus, as `CLAUDE.md` says), tick the phase's boxes here, add a line
under "Notes" for anything the next phase must know, and commit.

## Decisions

- **Library: Fluent through `i18n-embed` + `i18n-embed-fl`**, the stack
  COSMIC (also iced) uses. `fl!` checks keys and arguments against the
  English file at compile time; Fluent handles CLDR plurals and word
  order; `DesktopLanguageRequester` reads the system language on all three
  platforms. Dependencies are optional, behind the `gui` feature, and are
  recorded in `adr/0001`'s library table (or a short ADR 0002,
  "Localisation").
- **What is translated:** the app: UI, tray, notifications, dialogs,
  error messages. **Stays English:** the CLI, the HTTP API (it reports
  error codes, which the UI words), logs, the website.
- **Language:** the system's; en-US is the fallback. A daemon setting
  overrides it (phase 7), since preferences live in the daemon
  (HANDOFF ground rules). `FERRY_LANG` forces one, for testing.
- **Right-to-left layout is out of scope.** Fluent's bidi isolation marks
  stay on so an RTL device name can't scramble a sentence.
- **First languages:** Simplified Chinese (`zh-CN`, CJK fonts, no spaces)
  and German (`de`, long words). They are written by an agent and need a
  native speaker's review; say so in the file headers.
- **Translations arrive as PRs** to `i18n/<lang>/ferry.ftl`. (Weblate,
  which reads Fluent, can be added later by the owner.)

## Rules for extraction

- One `i18n/<lang>/ferry.ftl` per language, grouped by feature with a
  comment header per group; keys are prefixed by feature (`browse-…`,
  `clipboard-…`, `error-<code>` for `ui::error`).
- Whole sentences only. No verb or phrase passed into a sentence
  (`describe_file_failures(verb, …)`, `"Couldn’t {action}"`): one key per
  case.
- Every count goes through a Fluent plural selector; no `len() == 1`
  branches choosing words.
- Device names, file names, paths and numbers are arguments, never
  concatenated with translated text.
- Tests run in en-US whatever the machine's `LANG` is, and with isolation
  marks off, so `find("To Pixel · 3.0 MB")` still matches.

## Phases

### 1. Infrastructure
- [x] `i18n.toml`, `i18n/en-US/ferry.ftl`, dependencies (gui-only),
      library table in `adr/0001`
- [x] `ui::i18n`: a static loader, `fl!` re-exported, language chosen at
      boot in `launch.rs` (system, `FERRY_LANG` override, en-US fallback)
- [x] Tests pinned to en-US with isolation marks off (`ui::testing`,
      `tests/ui_e2e.rs`, unit tests)
- [x] A test that every locale's `.ftl` parses and has exactly en-US's keys
- [x] `cargo build -p ferry` still has no iced, and no i18n crates

### 2. Extract every string
Split into four commits, one per step, each passing the checks.
- [x] 2a. `ui::error` (codes to `error-<code>`), the shell, `widgets`,
      `actions`, `mod.rs`, `drops`, `launch`, and each feature's
      describe module
- [x] 2b. Pages: devices, device, pairing, add device, transfers,
      settings, about
- [x] 2c. Overlays (dialog, drop, incoming, toast) and features: share,
      clipboard, ping, findmyphone, battery, notifications, browse
- [x] 2d. Tray menu, desktop notifications (incl. the "Open" action),
      file-dialog titles, `background.rs`; then a sweep:
      `grep` `src/ui` for remaining user-visible literals

### 3. Numbers, dates, sizes
- [x] `format_timestamp` in the locale's date and time format (chrono's
      `unstable-locales`, or equivalent)
- [x] `format_bytes` with the locale's decimal separator and unit names
      from the `.ftl`
- [x] Percentages and other numbers through Fluent

### 4. Pseudo-locale
- [x] A generated `en-XA` (accented, ~40% longer, bracketed) built from
      en-US, not checked in by hand
- [x] Snapshot tests also render `en-XA` when `SNAPSHOT_DIR` is set; fix
      any unextracted string or clipped layout they show
- [ ] `FERRY_LANG=en-XA` works in the real app (unit-tested; not yet seen
      on screen, see Notes)

### 5. Platform integration
- [x] macOS: `CFBundleLocalizations` and a `<lang>.lproj` per shipped
      language in `packaging/macos`, so system dialogs follow the app;
      notification action titles localised (there are none; see Notes)
- [x] Linux: `Name[xx]`/`Comment[xx]` in the `.desktop` file; the `.deb`
      recommends a CJK font (`GenericName`, `Comment`, `Keywords`; the
      name stays "Ferry")
- [x] Windows: installer languages in `installer.nsi`
- [ ] Check CJK glyphs render (no boxes) in the app under Xvfb (seen in a
      macOS snapshot only, see Notes)

### 6. First languages and workflow
- [x] `i18n/zh-CN/ferry.ftl` and `i18n/de/ferry.ftl`, complete
- [x] Look at snapshots in both for clipping (`SNAPSHOT_LANGUAGES`; none
      found; bold CJK mixes fonts on macOS, see Notes)
- [x] HANDOFF.md and ARCHITECTURE.md: how strings work, how to add a
      string and a language

### 7. Language setting
- [x] A `language` daemon setting (none = system): core, `http.rs`,
      `client.rs`/CLI, then the Settings page, as the ground rules order
- [x] Switching it re-selects the loader at run time, rebuilds the tray
      menu, and updates the window without a restart (tested on the
      loader; not yet seen in the real app, see Notes)
- [x] ARCHITECTURE.md updated for the API change

## Notes

Anything a later phase must know, one line each, newest last.

- Phase 1: `use crate::ui::i18n::fl;` then `fl!("key", name = value)`;
  `LOADER` is the static loader. Only `drop-send-to-header` (drops.rs's
  tray chooser, a plural) is extracted so far, as the pilot; 2a does the
  rest of `drops`.
- Language selection runs only in `launch::run`, never in `program`, so
  unit tests (`cfg(test)` turns isolation off) and `tests/ui_e2e.rs`
  (`ui::i18n::use_test_language()` in `launch`) stay en-US without marks.
  `set_use_isolating` resets whenever languages are (re)loaded: phase 7's
  runtime switch must apply it again after `select`.
- The key test (`ui::i18n::tests`) reads every `i18n/*/ferry.ftl` from
  disk and compares message, term and attribute ids with en-US; a new
  locale directory needs no registration.
- Editing only an `.ftl` doesn't make cargo rebuild `fl!`'s compile-time
  check (debug builds read the files at run time through `rust-embed`);
  touch a `.rs` file if a stale key check confuses you.
- `i18n_embed::select` negotiates with `Filtering`: check in phase 6 that
  macOS's `zh-Hans-CN` and Windows' tags reach `zh-CN`, or map them.
- Phase 1 was checked on macOS (no Xvfb there): `ui_e2e` is Linux-only and
  didn't run, the real-clipboard tests were skipped, and `tests/lan.rs`'s
  loopback test fails on macOS (no `127.255.255.255`, HANDOFF "Traps"; no gui code in it).
- 2a: keys for error codes use the code verbatim, underscores and all
  (`error-device_not_found`, `browse-error-file_not_found`), so they grep
  to the API's code; codes that share a sentence share the first code's
  key. Other keys are kebab-case by area: `shell-` (mod.rs, actions.rs,
  shell.rs), `widget-`, `drop-`, `startup-`, `app-window-title`.
- 2a: `describe_file_failures` takes `error::FileBatch::{Send, Upload}`
  instead of a verb; `widgets::empty_state` takes owned `Option<String>`
  for its detail and action label, so pages can pass `fl!` results.
- 2a also did `pages/startup.rs` (launch's screen). Left for later on
  purpose: `format_bytes` (phase 3), `overlay::drop::CHOOSE_LABEL`, which
  `drops::drop_hint` uses (2c), and the 192.168.1.20 hint, which isn't
  words.
- 2a was checked on macOS, as phase 1 was; also `tests/transfer_e2e.rs`'s
  `zero_byte_small_and_larger_than_buffer_files_transfer_without_full_buffering`
  fails there (a transfer ends `Failed`), without the `gui` feature, so
  not from this work; look on Linux.
- 2b: page keys are prefixed by page (`devices-`, `device-`, `add-device-`,
  `pairing-`, `transfers-`, `settings-`, `about-`); `device-reachability-*`
  is shared by the device list and Add device. `reachability_label`,
  `add_device::blocker`, `device::type_name` and `pairing::describe` now
  return `String`. Transfer failures are `transfers-failed-<code>` (the
  core's `OperationErrorCode`, snake case).
- 2b: a transfer row names its device with `transfers-from`/`transfers-to`
  (`From { $name } · { $status }`), a whole string, not a preposition
  glued on. Sizes in `transfers-progress` still come from `format_bytes`
  (phase 3). The app's name (`about::NAME`, "Ferry"), the About page's
  URLs and the protocol version stay untranslated.
- 2b left for 2c: the device page's action buttons and status chips and
  the Settings page's feature sections take their labels from the
  features (`DeviceAction::label`, `DeviceStatus::label`,
  `settings_sections`), so they are extracted with each feature.
- 2b was checked on macOS, as 2a was: same `tests/lan.rs` loopback failure
  and skipped real-clipboard tests; `transfer_e2e` passed this time. The
  pages' snapshots (`SNAPSHOT_DIR`, `-p ferry --features gui`) read as
  before.
- 2c: overlay keys are `dialog-` (the shared Cancel), `drop-choose-hint`
  and `drop-chooser-…`, `incoming-`; feature keys are `ping-`,
  `findmyphone-`, `battery-`, `share-`, `clipboard-`, `notifications-`,
  `browse-`, each feature's errors now in its own group.
  `overlay::drop::CHOOSE_LABEL` and browse's `CANT_SHOW` are functions
  now (`choose_label()`, `cant_show()`); `invalid_name_reason` returns
  `Option<String>`.
- 2c: the chooser's title for exactly one file ("Send file", the name
  under it) is its own key, `drop-chooser-title-one`; the plural key
  still has a `[one]` for languages whose `one` covers 21. The
  notifications action uses a `[0]` variant for its unnumbered label.
- 2c: a notification's button that failed now reads "Couldn’t do
  “{ $action }”" (it was "Couldn’t {action}", a verb glued on); the
  label is the phone's own text.
- 2c left on purpose: `battery-charge` is `{ $charge }%` with the number
  unformatted, and the dialog's `n/max` counter is plain digits (both
  phase 3); `--demo`'s made-up notifications (`notifications::demo_packets`)
  stand for what a phone sends, so they stay English; the share picker's
  title (`share-pick-title`) and browse's (`browse-upload-title`) are
  done, so 2d's file-dialog titles are only the shell's own.
- 2c was checked on macOS, as 2b was: the same `tests/lan.rs` loopback
  failure and skipped real-clipboard tests; `transfer_e2e`'s
  zero-byte test failed again, also with `-p ferry` alone (no gui code).
  The overlays' and browse's snapshots read as before.
- Merged main after 2c (f50e586). It brought: About's third-party licenses
  (#46), with new English strings for 2d's sweep to catch; the bundled
  Figtree font (#45), which has no CJK glyphs, so phase 5 must check
  fallback to a system CJK font; and SQLite-backed typed configs (#47),
  which is where phase 7's `language` setting goes.
- 2d: keys are `tray-` (the menu; a device's submenu is `tray-device-status`,
  `{ $name } · { $status }`) and `notify-` (the pairing and received-file
  notifications, and Linux's "Open" button, looked up per notification in
  `desktop::notify`'s D-Bus loop, so phase 7 needs nothing there). About's
  licenses row (from main's #46) is `about-licenses`. File-dialog titles
  were already done (`shell-download-dir-title`, 2c's pickers).
- 2d sweep: what's left in `src/ui` on purpose is the app's name ("Ferry":
  tray tooltip, SNI title, notification app name, `about::NAME`), `--demo`'s
  made-up device names and notifications, the fake phone's storages,
  `format_bytes`' units (phase 3), and errors only logged (`open`,
  `autostart`, which the UI words as `shell-open-failed` and
  `shell-start-on-login-failed`). macOS's app menu (winit's default "Quit
  Ferry" etc.) isn't ours; phase 5's `.lproj` covers it.
- 2d: the tray menu is rebuilt with `fl!` on each `update_tray` and sent
  when its layout (labels included) changes, so phase 7 only has to call
  `update_tray` after switching language.
- 2d was checked on macOS, as 2c was: the same `tests/lan.rs` loopback
  failure and skipped real-clipboard tests; `transfer_e2e` passed. The
  Linux-only code (`desktop::notify`'s D-Bus, the SNI tray) was
  type-checked with `cargo clippy --target x86_64-unknown-linux-musl` and
  HANDOFF's fake `cc`/`ar`; the real app wasn't run (no Xvfb on macOS).
- 3: numbers and dates come from ICU4X (`icu_decimal`, `icu_datetime`;
  ADR 0001's table says why not chrono's `unstable-locales`), in
  `ui::i18n::format`. Every number a message is given is written by
  ICU4X through Fluent's `set_formatter` hook, which `i18n::select`
  reinstalls, like isolation, after (re)loading: phase 7's switch gets it
  by going through `select`. For fixed fraction digits pass
  `format::decimal(value, digits)` as the argument; a plain integer gets
  the locale's grouping ("1,023 bytes"). Units and the percent sign stay
  in the message (`widget-size-*`, `battery-charge`), so each language
  spaces them its own way ("82 %" in German).
- 3: the formatting locale is the first requested one that speaks the
  translation's language, else the translation's (`en-GB` gets British
  dates with the en-US text; `fr` with no French gets en-US). Phase 7's
  setting should pass the chosen language as `requested`. Dates are the
  medium date with hours and minutes ("Sep 24, 2026, 2:03 PM",
  "24.09.2026, 14:03", "2026年9月24日 14:03"); browse's Modified column
  (168 px) fits en-US's; check zh-CN and de in phase 6's snapshots.
- 3: CLDR's narrow no-break space (U+202F, before "PM" and in French
  grouping) is turned into U+00A0, introduced for the earlier Figtree font
  which had no glyph for it; Inter supports it, but the normalization remains;
  tests expecting a time use `\u{A0}`. `1 bytes` is now `1 byte`
  (plural), and counts over 999 are grouped.
- 3: new keys `widget-size-bytes`/`-kb`/`-mb`/`-gb`/`-tb` and
  `dialog-counter` (`{ $count }/{ $max }`). The About page's protocol
  version and the app's version stay strings, so they aren't grouped;
  pass any future port or year as a string for the same reason.
- 3 was checked on macOS, as 2d was: the same `tests/lan.rs` loopback
  failure, `transfer_e2e` passed, `ui_e2e` doesn't run there. Snapshots
  (browse's folder, transfers, the prompt's counter) read right in
  en-US. The real app wasn't run (no Xvfb on macOS), and no locale but
  en-US was seen on screen: only unit tests cover de, fr, en-GB and
  zh-CN formats. Release binary size with ICU4X's data wasn't measured.
- Merged main after phase 3. It brought the Settings page's "Command
  line access" section (`src/ui/pages/settings.rs`, `CliCopy` in
  `src/ui/actions.rs`: "Setup copied", "Token copied", "Copy setup",
  "Copy token", "New token", "Let ferry-cli control this app",
  "Couldn’t change command line access: …") in English. Phase 4 must
  extract these first, then use the pseudo-locale to catch anything else.
  The CLI binary is now `ferry-cli` (`src/bin/ferry-cli`).
- 4: first extracted main's "Command line access" section (`settings-cli-…`
  keys). The API's own error (`ApiStatus::error`, English from the OS)
  now sits in a sentence, `settings-cli-not-listening`.
- 4: en-XA is made at run time from the embedded en-US file
  (`ui::i18n::pseudo`, `fluent-syntax` parses and re-serializes it), so
  there is no `i18n/en-XA/` and the key test doesn't see it. It is offered
  only when `en-XA` is requested by name (`pseudo::is_requested`): with
  Fluent's `Filtering`, a system asking for `en-GB` could otherwise land on
  it. Phase 7's language list must leave it out, or show it only for
  testing. Brackets are `{ "[" }` string literals, which Fluent doesn't
  isolate. The accented letters are all Latin-1/Extended-A (Inter has
  them); numbers format as English.
- 4: unit tests render en-XA through `ui::i18n::in_pseudo_locale`, which
  makes `LOADER` (now a `Deref` wrapper, `ui::i18n::Loader`) return an
  en-XA loader on that thread only, so parallel tests stay en-US.
  `testing::snapshot` keeps it on through layout, as `responsive` builds
  its content then, and writes `<name>-en-XA-light-<backend>.png`
  (isolation marks on, as in the app; they render as nothing).
- 4: what the en-XA snapshots still show in English is test data (the
  fake features "Wave"/"Hug"/"Waving"/"N bars" and "Sync clipboard" passed
  in by tests, dialogs and toasts built with literal text, device, file
  and storage names), and errors from outside the app (the startup
  screen's OS error). Nothing unextracted was found. One layout fix: the
  device list's "This computer: …" line was cut off beside "Add device"
  (`Wrapping::None` + clip); it wraps now.
- 4 was checked on macOS, as 3 was: the same `tests/lan.rs` loopback
  failure, `ui_e2e` doesn't run there. The real app wasn't run (no Xvfb
  on macOS): run `FERRY_LANG=en-XA ... cargo run -p ferry-gui -- --demo`
  under Xvfb on Linux and look at the window, the tray menu and a
  notification. The en-XA path is covered by unit tests
  (`ui::i18n::tests::en_xa_is_loaded_only_when_asked_for_by_name`).
- 5: what the system shows about the app outside it comes from `package-*`
  messages (one plain line each; `ui::i18n::tests` checks), which
  `packaging/i18n.sh` copies into the packages for every `i18n/<lang>/`:
  the `.desktop` entry's `GenericName`/`Comment`/`Keywords` (and their
  `[zh_CN]`-style variants; the checked-in English is overwritten from
  en-US), macOS's `<lang>.lproj/InfoPlist.strings` (the local network
  prompt) and `CFBundleLocalizations`, and an NSIS include
  (`MUI_LANGUAGE` per language, `LangString package_start_app`) passed as
  `/DLANGUAGES`. A language is shipped by having its directory; phase 6
  must translate `package-*` too, and a language NSIS's table in
  `i18n.sh` lacks stops the Windows build until it is added. zh-CN's
  lproj is `zh-Hans`, en-US's is `en`.
- 5: macOS notifications have no action buttons of ours (a click is the
  default action), so only the system's own "Options"/"Close" show, and
  those follow the bundle's localizations. winit 0.30's macOS app menu
  ("Hide Ferry", "Quit Ferry", "Services"; `platform_impl/macos/menu.rs`)
  is hard-coded English, so the `.lproj` doesn't reach it (2d's note was
  wrong): localising it means setting our own `NSApp` main menu once the
  window opens. Not done.
- 5: the Linux autostart entry's comment is `shell-autostart-comment`,
  written in the language of the moment it is turned on; phase 7's switch
  could rewrite it while it is on.
- 5: `devices-cjk` (a snapshot in `pages/devices.rs`) renders CJK device
  names. On macOS they came out right (PingFang, by cosmic-text's
  fallback). Under Xvfb on Linux it wasn't run (no Xvfb on macOS): run it
  and the app there, with and without `fonts-noto-cjk`. cosmic-text picks
  the CJK fallback by the system's locale (`sys-locale`), not the app's
  language, so on an English system zh-CN text may get Japanese glyph
  forms; check in phase 6.
- 5 was checked on macOS, as 4 was: the same `tests/lan.rs` loopback
  failure, `ui_e2e` doesn't run there; the Linux code was clippy-checked
  for `x86_64-unknown-linux-musl`. `i18n.sh` was tried with made-up de
  and zh-CN files (quotes, `$` and `\` in them; `plutil -lint` passes),
  but `build_deb.sh` (with `check_deb.sh`'s `desktop-file-validate`) and
  `makensis` weren't run here: the Build workflow runs both.
- 6: de and zh-CN are complete and written by an agent; their headers ask
  for a native speaker's review. German uses "du" and „…“ quotes, "koppeln"/
  "Kopplung" for pairing and "Infobereich" for the tray; Chinese uses full-width punctuation, no plural selectors, and
  keeps "Ping" as the action's name. Both keep English `package-keywords`
  after their own so English searches still find the app.
- 6: `i18n_embed::select` already negotiates `zh-Hans-CN`, `zh-Hans`,
  `zh-SG` to zh-CN and `de-DE`/`de-AT`/`de-CH` to de, so no mapping was
  needed (`ui::i18n::tests::system_tags_reach_the_translations`).
- 6: `ui::i18n::in_locale(tag, f)` generalises `in_pseudo_locale`: `fl!`
  on that thread uses that language (isolation marks on) and
  `i18n::format` its locale (`format::set_thread_locale`, test-only).
  `SNAPSHOT_LANGUAGES=de,zh-CN` renders every snapshot in those too, as
  `<name>-<tag>-light-<backend>.png`.
- 6: the snapshots show no clipping in either language (German's
  "Weiterlaufen, wenn das Fenster geschlossen wird" and "Neues Token"
  wrap; browse's Modified column fits "24.09.2026, 14:03" and
  "2026年9月24日 14:03"). What stays English there is test data, as in 4.
- 6: on macOS, bold CJK text (page titles, dialog titles, column headers)
  mixes fonts glyph by glyph, some from a serif (Songti) face: PingFang has
  no 700 face, and cosmic-text 0.15's fallback only takes fonts of the
  exact weight, so each glyph lands on whichever Bold font has it. Regular
  text is fine. Not fixed (iced is pinned; HANDOFF "Traps"); options are a
  cosmic-text fix upstream, or drawing titles in Semibold with an Inter
  Semibold face bundled. Linux's Noto Sans CJK and Windows' YaHei have
  bold faces, so check there before deciding.
- 6 was checked on macOS, as 5 was: the same `tests/lan.rs` loopback
  failure, real-clipboard tests skipped, `ui_e2e` doesn't run there. The
  real app wasn't run in de or zh-CN (no Xvfb); `packaging/i18n.sh`'s
  output for both was checked (`plutil -lint` passes), `makensis` and
  `desktop-file-validate` weren't run.
- 7: the setting is `ui.language` (`core::settings::LANGUAGE`), `language`
  in `GET/PATCH /settings`: a BCP 47 tag, or `null` for the system's. The
  daemon checks only its shape (`invalid_settings` otherwise), not that a
  translation exists; the UI negotiates it like a system tag, so `de-AT`
  gets German and an unknown one en-US. `http.rs`/`client.rs` needed no
  change (they pass the whole patch). CLI: `ferry-cli settings --language
  <tag|system>`, and `settings` prints it.
- 7: `i18n::follow_setting`, called from `App::update` after every
  message (before `update_tray`), reloads `LOADER` through `select` when
  the setting changed, then restores isolation (off only after
  `use_test_language`). It does nothing until `select_system_language` or
  `use_test_language` ran, so unit tests stay en-US; `FERRY_LANG` still
  wins over the setting. A chosen tag is requested alone, so dates follow
  it (`de` gives German dates even on a `de-AT` system). When it switches
  while starting on login is on, the login item is rewritten (Linux's
  comment, 5's note).
- 7: Settings has a Language row with a `pick_list`: "System default",
  then every `i18n/` language by its new `settings-language-own-name`
  message (a new language needs only that message). A tag set from the
  CLI that isn't listed (`de-AT`, `en-XA`) shows as the placeholder. The
  list draws its own text, which `iced_test`'s `Simulator` can't find or
  click, so its choices are unit-tested and the snapshots show it (fine
  in de, zh-CN and en-XA).
- 7: until the daemon's settings are read at start, the app shows the
  system's language, so a chosen one replaces it a moment after launch
  (the startup screen and the first tray menu). Text already on screen
  that was built as a value (an open toast or dialog) stays in the old
  language until it closes; pages, the title and the tray follow at once.
- 7 was checked on macOS, as 6 was: the same `tests/lan.rs` loopback
  failure, real-clipboard tests skipped, `ui_e2e` doesn't run there.
  `tests/i18n_switch.rs` switches the real loader (en-US, de, zh-CN,
  en-XA, back). The real app wasn't run (no Xvfb): on Linux, run it with
  `--api-port`, `ferry-cli settings --language de` against it, and check
  that the window, its title, the tray menu and the next notification
  switch without a restart, and that the Settings list switches it too.

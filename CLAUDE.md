# Agent instructions

Start with [`docs/HANDOFF.md`](docs/HANDOFF.md): ground rules, the "done
means" checks, and how to verify in the real app.

## Isolate every run

Several agents may work here at once, each in its own git worktree, and
the owner runs the real app on the same machine. Anything you launch (the
app, a CLI daemon, integration tests) must not touch shared state or
collide with another run. Always:

- **Data.** Give every daemon a fresh `mktemp -d` directory under your
  scratchpad, never the default config dir (`~/.config/ferry`, the owner's
  real identity and trust) or a fixed path like `/tmp/ui` another session
  may use:
  - CLI and the app (`ferry-gui`): `--data-dir "$dir/data"
    --download-dir "$dir/downloads"`. Without the download dir, received
    files land in the owner's `~/Downloads`.
  - `tests/ui_e2e.rs` and the other integration tests make their own.

  Delete the directory when you are done.
- **Ports.** Don't use the default API port 24816 or a port from the docs
  (25011). Pick a free one, e.g.
  `python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'`.
  The app serves no API unless given `--api-port` (pass a free one) or
  Settings → Command line access is on, which listens on 24816 unless
  its store says otherwise: don't switch it on in a test run that
  wasn't given `--api-port`.
- **Network.** Pass `--discovery-loopback` (CLI and app) so nothing
  announces on or listens to the LAN: discovery binds `127.255.255.255`
  and the control and payload ports bind `127.0.0.1`, so remote devices
  can neither find nor dial the instance (`ss -lunpt` shows only loopback
  addresses for its PID). That doesn't hide it from this machine: the
  owner's Ferry or KDE Connect binds UDP `0.0.0.0:1716`, which on Linux
  also receives broadcasts to `127.255.255.255:1716`, so it would list and
  dial your instance. So also pass `--discovery-port` with a free UDP port
  (CLI and app; `FERRY_DISCOVERY_PORT` for the app and
  `examples/fake_phone.rs`), the same for every instance of your run:
  instances meet only on the same port. `tests/ui_e2e.rs` picks its own.
  Other sessions' loopback instances on the same port can still see yours
  in a scan, so pair only with the device id you started, never by name.
  Don't pair with or send to real devices without asking.
- **Display and D-Bus.** Run the app under
  `dbus-run-session -- xvfb-run --auto-servernum ...`, or on an `Xvfb`
  display number you checked is free, with `WAYLAND_DISPLAY` unset. The
  private bus keeps the tray, notifications and file dialogs off the
  owner's desktop; the private data dir keeps the app's single-instance
  socket apart from theirs (otherwise it would just show their running app
  and exit). Run `cargo test` the same way: the
  `plugins::clipboard::backend::system` tests use the real clipboard, so
  on the owner's display they clobber it and fail when it changes under
  them. The whole recipe:

  ```sh
  dir=$(mktemp -d -p "$scratchpad")
  udp=$(python3 -c 'import socket; s=socket.socket(type=socket.SOCK_DGRAM); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
  env -u WAYLAND_DISPLAY \
    dbus-run-session -- xvfb-run --auto-servernum \
    cargo run -p ferry-gui -- --discovery-loopback --discovery-port "$udp" \
      --data-dir "$dir/data" --download-dir "$dir/downloads"
  ```

  Add `--demo` for made-up devices, and `--api-port`/`--api-token` to
  drive the app's daemon from the CLI (`ferry-cli`; with `--data-dir
  "$dir/data"` it reads the app's token from there).
- **Processes.** Keep the PIDs you start and kill those, not
  `pkill -f <pattern>`, which can hit another session's processes.

## Worktrees

Each worktree builds its own `target/`, so the first build is slow.

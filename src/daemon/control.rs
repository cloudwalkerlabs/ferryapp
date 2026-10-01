//! The daemon's control socket ([`crate::rpc`]), which the app can turn on
//! and off while it runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[cfg(unix)]
use crate::rpc::RpcServer;
use crate::{
    config::{COMMAND_LINE_ACCESS, CommandLineAccess},
    core::Core,
    store::Store,
};

/// How a daemon serves its control socket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ControlMode {
    /// For the whole run, as `ferry-cli run` does: the socket is how it is
    /// controlled, and failing to listen fails the start (except on Windows,
    /// which has no socket yet).
    #[default]
    Always,
    /// As the store's [`COMMAND_LINE_ACCESS`] says, and switched with
    /// [`ControlSwitch::set_enabled`]: the app's. Failing to listen leaves
    /// it off, and [`ControlStatus::error`] says why.
    Stored {
        /// On for this run whatever is stored, as when the app is given
        /// `--cli-access`.
        force: bool,
    },
}

/// The control socket as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlStatus {
    /// Meant to serve: switched on, or on for the whole run.
    pub enabled: bool,
    /// [`ControlSwitch::set_enabled`] can change it ([`ControlMode::Stored`]).
    pub switchable: bool,
    /// Where it listens, or would.
    pub path: PathBuf,
    /// Whether it listens now.
    pub listening: bool,
    /// Why it doesn't listen although it is enabled.
    pub error: Option<String>,
}

/// Starts and stops the daemon's control socket. Cheap to clone; every
/// clone switches the same server.
#[derive(Clone)]
pub struct ControlSwitch(Arc<Inner>);

struct Inner {
    core: Core,
    path: PathBuf,
    /// The service's; each server stops on a child of it.
    shutdown: CancellationToken,
    /// Where the choice is kept: `None` for [`ControlMode::Always`].
    store: Option<Store>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stored: CommandLineAccess,
    /// On for this run, whatever is stored.
    forced: bool,
    #[cfg(unix)]
    server: Option<RpcServer>,
    error: Option<String>,
}

impl ControlSwitch {
    /// Serve at `path` as `mode` says. With [`ControlMode::Always`], an
    /// error means it couldn't listen.
    pub(crate) async fn start(
        mode: ControlMode,
        store: Store,
        core: Core,
        path: PathBuf,
        shutdown: CancellationToken,
    ) -> Result<Self> {
        let (store, state) = match mode {
            ControlMode::Always => (
                None,
                State {
                    forced: true,
                    ..State::default()
                },
            ),
            ControlMode::Stored { force } => {
                let stored = store
                    .get(&COMMAND_LINE_ACCESS)
                    .await
                    .unwrap_or_else(|error| {
                        // Off rather than not starting; it is rewritten
                        // when the user turns it on.
                        warn!(%error, "ignoring unreadable command line access setting");
                        None
                    })
                    .unwrap_or_default();
                (
                    Some(store),
                    State {
                        stored,
                        forced: force,
                        ..State::default()
                    },
                )
            }
        };
        let always = store.is_none();
        let switch = Self(Arc::new(Inner {
            core,
            path,
            shutdown,
            store,
            state: Mutex::new(state),
        }));
        {
            let mut state = switch.0.state.lock().await;
            if state.enabled()
                && let Err(error) = switch.0.listen(&mut state).await
            {
                // Where there is no socket to serve (Windows), a CLI daemon
                // still runs, uncontrolled, as the app does.
                if always && cfg!(unix) {
                    return Err(error);
                }
                warn!(error = %format!("{error:#}"), "command line access is off");
                state.error = Some(format!("{error:#}"));
            }
        }
        Ok(switch)
    }

    /// Off, and not switchable: for UI tests that don't use it.
    #[cfg(all(test, feature = "gui"))]
    pub(crate) fn unavailable(core: Core) -> Self {
        Self(Arc::new(Inner {
            core,
            path: PathBuf::new(),
            shutdown: CancellationToken::new(),
            store: None,
            state: Mutex::new(State::default()),
        }))
    }

    pub async fn status(&self) -> ControlStatus {
        let state = self.0.state.lock().await;
        self.0.status(&state)
    }

    /// Where it listens, or would.
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Turn the socket on or off, and keep the choice for the next start.
    /// If it can't listen (e.g. another Ferry serves this data directory),
    /// nothing changes and the error says why.
    pub async fn set_enabled(&self, enabled: bool) -> Result<ControlStatus> {
        let store = self
            .0
            .store
            .as_ref()
            .context("command line access is on for this run")?;
        let mut state = self.0.state.lock().await;
        let stored = CommandLineAccess { enabled };
        if enabled {
            if !self.0.listening(&state) {
                self.0.listen(&mut state).await?;
            }
            if let Err(error) = store.set(&COMMAND_LINE_ACCESS, &stored).await {
                self.0.stop(&mut state).await;
                return Err(error).context("couldn't save the command line access setting");
            }
        } else {
            store
                .set(&COMMAND_LINE_ACCESS, &stored)
                .await
                .context("couldn't save the command line access setting")?;
            // Switching off also ends this run's `--cli-access`.
            state.forced = false;
            self.0.stop(&mut state).await;
        }
        state.stored = stored;
        state.error = None;
        info!(enabled, "command line access switched");
        Ok(self.0.status(&state))
    }

    pub(crate) async fn shutdown(&self) {
        let mut state = self.0.state.lock().await;
        self.0.stop(&mut state).await;
    }
}

impl State {
    fn enabled(&self) -> bool {
        self.forced || self.stored.enabled
    }
}

impl Inner {
    fn status(&self, state: &State) -> ControlStatus {
        ControlStatus {
            enabled: state.enabled(),
            switchable: self.store.is_some(),
            path: self.path.clone(),
            listening: self.listening(state),
            error: state.error.clone(),
        }
    }

    #[cfg(unix)]
    fn listening(&self, state: &State) -> bool {
        state.server.is_some()
    }

    #[cfg(not(unix))]
    fn listening(&self, _state: &State) -> bool {
        false
    }

    #[cfg(unix)]
    async fn listen(&self, state: &mut State) -> Result<()> {
        let server = RpcServer::start(
            self.path.clone(),
            crate::rpc::all(&self.core),
            self.shutdown.child_token(),
        )
        .await?;
        state.server = Some(server);
        state.error = None;
        Ok(())
    }

    #[cfg(not(unix))]
    async fn listen(&self, _state: &mut State) -> Result<()> {
        let _ = (&self.core, &self.shutdown);
        Err(anyhow::anyhow!(
            "command line access isn't available on this platform yet"
        ))
    }

    async fn stop(&self, state: &mut State) {
        #[cfg(unix)]
        if let Some(server) = state.server.take() {
            server.shutdown().await;
        }
        #[cfg(not(unix))]
        let _ = state;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };

    use super::*;
    use crate::core::testing;

    async fn test_core() -> Core {
        testing::handle().await.0
    }

    async fn start(mode: ControlMode, store: &Store, path: &Path) -> Result<ControlSwitch> {
        ControlSwitch::start(
            mode,
            store.clone(),
            test_core().await,
            path.to_owned(),
            CancellationToken::new(),
        )
        .await
    }

    /// The daemon's answer to `status`, or `None` if nothing listens.
    async fn status_answer(path: &Path) -> Option<String> {
        let mut stream = UnixStream::connect(path).await.ok()?;
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\"}\n")
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.ok()?;
        Some(line)
    }

    #[tokio::test]
    async fn the_stored_switch_starts_off_and_stays_as_it_was_left() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ferry.sock");
        let store = Store::open_in_memory().await.unwrap();
        let switch = start(ControlMode::Stored { force: false }, &store, &path)
            .await
            .unwrap();
        let status = switch.status().await;
        assert!(!status.enabled && status.switchable && !status.listening);
        assert!(!path.exists());

        let on = switch.set_enabled(true).await.unwrap();
        assert!(on.enabled && on.listening);
        assert!(status_answer(&path).await.unwrap().contains("\"result\""));
        assert!(
            store
                .get(&COMMAND_LINE_ACCESS)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );

        // It comes back on after a restart.
        switch.shutdown().await;
        assert!(!path.exists(), "the socket is removed when it stops");
        let switch = start(ControlMode::Stored { force: false }, &store, &path)
            .await
            .unwrap();
        assert!(switch.status().await.listening);

        let off = switch.set_enabled(false).await.unwrap();
        assert!(!off.enabled && !off.listening);
        assert!(!path.exists());
        assert!(
            !store
                .get(&COMMAND_LINE_ACCESS)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[tokio::test]
    async fn it_wont_turn_on_while_another_daemon_serves_the_socket() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ferry.sock");
        let other_store = Store::open_in_memory().await.unwrap();
        let other = start(ControlMode::Always, &other_store, &path)
            .await
            .unwrap();

        let store = Store::open_in_memory().await.unwrap();
        let switch = start(ControlMode::Stored { force: false }, &store, &path)
            .await
            .unwrap();
        let error = switch.set_enabled(true).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("already serving"),
            "{error:#}"
        );
        let status = switch.status().await;
        assert!(!status.enabled && !status.listening);
        assert_eq!(store.get(&COMMAND_LINE_ACCESS).await.unwrap(), None);

        // Stored as on, it starts anyway, off, with the reason.
        store
            .set(&COMMAND_LINE_ACCESS, &CommandLineAccess { enabled: true })
            .await
            .unwrap();
        let status = start(ControlMode::Stored { force: false }, &store, &path)
            .await
            .unwrap()
            .status()
            .await;
        assert!(status.enabled && !status.listening);
        assert!(status.error.unwrap().contains("already serving"));

        // A daemon that must serve doesn't start.
        let error = start(ControlMode::Always, &store, &path)
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("already serving"),
            "{error:#}"
        );
        // The one serving is unharmed.
        assert!(status_answer(&path).await.is_some());
        other.shutdown().await;
    }

    #[tokio::test]
    async fn forced_on_for_the_run_without_storing_that() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ferry.sock");
        let store = Store::open_in_memory().await.unwrap();
        let switch = start(ControlMode::Stored { force: true }, &store, &path)
            .await
            .unwrap();
        let status = switch.status().await;
        assert!(status.enabled && status.listening);
        assert_eq!(store.get(&COMMAND_LINE_ACCESS).await.unwrap(), None);
        switch.shutdown().await;
    }

    #[tokio::test]
    async fn an_always_on_socket_cant_be_switched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ferry.sock");
        let store = Store::open_in_memory().await.unwrap();
        let switch = start(ControlMode::Always, &store, &path).await.unwrap();
        let status = switch.status().await;
        assert!(status.enabled && status.listening && !status.switchable);
        assert!(switch.set_enabled(false).await.is_err());
        assert_eq!(store.get(&COMMAND_LINE_ACCESS).await.unwrap(), None);
        switch.shutdown().await;
    }
}

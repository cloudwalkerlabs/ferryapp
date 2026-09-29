//! The daemon's HTTP API, which the app can turn on and off while it runs.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    api::{ApiServer, ApiServerConfig, DEFAULT_API_PORT},
    config::{API, ApiToken, StoredApi},
    core::Core,
    store::Store,
};

/// How a daemon serves its HTTP API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiMode {
    /// For the whole run, as `ferry-cli run` does: the API is how it is
    /// controlled. Failing to listen fails the start. With no token, clients
    /// need none.
    Always {
        host: IpAddr,
        /// `0` picks a free port.
        port: u16,
        token: Option<ApiToken>,
    },
    /// As the store's [`API`] entry says, on loopback, and switched
    /// with [`ApiSwitch`]: the app's. Always with a token, the stored one
    /// (made when first needed). Failing to listen leaves it off, and
    /// [`ApiStatus::error`] says why.
    Stored {
        /// Serve on this port for this run whatever is stored, as when the
        /// app is given `--api-port`. `0` picks a free port.
        port: Option<u16>,
        /// Use this token for this run instead of the stored one.
        token: Option<ApiToken>,
    },
}

impl Default for ApiMode {
    fn default() -> Self {
        Self::Always {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_API_PORT,
            token: None,
        }
    }
}

/// The API as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiStatus {
    /// Meant to serve: switched on, or on for the whole run.
    pub enabled: bool,
    /// [`ApiSwitch::set_enabled`] can change it ([`ApiMode::Stored`]).
    pub switchable: bool,
    /// The port it serves on, or would.
    pub port: u16,
    /// Where it listens, while it does.
    pub address: Option<SocketAddr>,
    /// What clients must present, if anything.
    pub token: Option<ApiToken>,
    /// Why it doesn't listen although it is enabled.
    pub error: Option<String>,
}

/// Starts and stops the daemon's HTTP API. Cheap to clone; every clone
/// switches the same server.
#[derive(Clone)]
pub struct ApiSwitch(Arc<Inner>);

struct Inner {
    core: Core,
    host: IpAddr,
    /// The service's; each server stops on a child of it.
    shutdown: CancellationToken,
    /// Where the choice is kept: `None` for [`ApiMode::Always`].
    store: Option<Store>,
    state: Mutex<State>,
}

struct State {
    stored: StoredApi,
    /// On for this run on this port, whatever is stored.
    port_override: Option<u16>,
    token: Option<ApiToken>,
    server: Option<ApiServer>,
    error: Option<String>,
}

impl ApiSwitch {
    /// Serve as `mode` says. With [`ApiMode::Always`], an error means it
    /// couldn't listen; with [`ApiMode::Stored`], only that the store
    /// couldn't be written.
    pub(crate) async fn start(
        mode: ApiMode,
        store: Store,
        core: Core,
        shutdown: CancellationToken,
    ) -> Result<Self> {
        let (host, store, state) = match mode {
            ApiMode::Always { host, port, token } => (
                host,
                None,
                State {
                    stored: StoredApi {
                        enabled: true,
                        port: Some(port),
                        ..StoredApi::default()
                    },
                    port_override: None,
                    token,
                    server: None,
                    error: None,
                },
            ),
            ApiMode::Stored { port, token } => {
                let stored = store
                    .get(&API)
                    .await
                    .unwrap_or_else(|error| {
                        // Off rather than not starting; it is rewritten
                        // when the user turns it on.
                        warn!(%error, "ignoring unreadable API settings");
                        None
                    })
                    .unwrap_or_default();
                (
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    Some(store),
                    State {
                        stored,
                        port_override: port,
                        token,
                        server: None,
                        error: None,
                    },
                )
            }
        };
        let always = store.is_none();
        let switch = Self(Arc::new(Inner {
            core,
            host,
            shutdown,
            store,
            state: Mutex::new(state),
        }));
        {
            let mut state = switch.0.state.lock().await;
            if state.enabled() {
                if !always {
                    switch.0.ensure_token(&mut state).await?;
                }
                if let Err(error) = switch.0.listen(&mut state).await {
                    if always {
                        return Err(error);
                    }
                    warn!(error = %format!("{error:#}"), "the HTTP API is off");
                    state.error = Some(format!("{error:#}"));
                }
            }
        }
        Ok(switch)
    }

    /// Off, and not switchable: for UI tests that don't use it.
    #[cfg(all(test, feature = "gui"))]
    pub(crate) fn unavailable(core: Core) -> Self {
        Self(Arc::new(Inner {
            core,
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            shutdown: CancellationToken::new(),
            store: None,
            state: Mutex::new(State {
                stored: StoredApi::default(),
                port_override: None,
                token: None,
                server: None,
                error: None,
            }),
        }))
    }

    pub async fn status(&self) -> ApiStatus {
        let state = self.0.state.lock().await;
        self.0.status(&state)
    }

    /// Where it listens, while it does.
    pub async fn address(&self) -> Option<SocketAddr> {
        let state = self.0.state.lock().await;
        state.server.as_ref().map(ApiServer::local_addr)
    }

    /// Turn the API on or off, and keep the choice for the next start.
    /// Turning it on makes a token if there is none yet. If it can't listen,
    /// nothing changes and the error says why.
    pub async fn set_enabled(&self, enabled: bool) -> Result<ApiStatus> {
        let store = self
            .0
            .store
            .as_ref()
            .context("the API is on for this run")?;
        let mut state = self.0.state.lock().await;
        let mut stored = state.stored.clone();
        stored.enabled = enabled;
        if enabled {
            self.0.ensure_token(&mut state).await?;
            if state.server.is_none() {
                self.0.listen(&mut state).await?;
            }
            // `ensure_token` may have stored a new token.
            stored = state.stored.clone();
            stored.enabled = true;
            if let Err(error) = store.set(&API, &stored).await {
                self.0.stop(&mut state).await;
                return Err(error).context("couldn't save the API settings");
            }
        } else {
            store
                .set(&API, &stored)
                .await
                .context("couldn't save the API settings")?;
            // Switching off also ends this run's `--api-port`.
            state.port_override = None;
            self.0.stop(&mut state).await;
        }
        state.stored = stored;
        state.error = None;
        info!(enabled, "HTTP API switched");
        Ok(self.0.status(&state))
    }

    /// Replace the token with a new random one and keep it, so clients set
    /// up with the old one stop working. A running server restarts with it.
    pub async fn new_token(&self) -> Result<ApiStatus> {
        let store = self
            .0
            .store
            .as_ref()
            .context("the API's token is fixed for this run")?;
        let mut state = self.0.state.lock().await;
        let token = ApiToken::generate();
        let mut stored = state.stored.clone();
        stored.set_token(&token);
        store
            .set(&API, &stored)
            .await
            .context("couldn't save the API settings")?;
        state.stored = stored;
        state.token = Some(token);
        if state.server.is_some() {
            self.0.stop(&mut state).await;
            if let Err(error) = self.0.listen(&mut state).await {
                state.error = Some(format!("{error:#}"));
                return Err(error);
            }
        }
        info!("HTTP API token replaced");
        Ok(self.0.status(&state))
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let mut state = self.0.state.lock().await;
        match state.server.take() {
            Some(server) => server.shutdown().await.map_err(Into::into),
            None => Ok(()),
        }
    }
}

impl State {
    fn enabled(&self) -> bool {
        self.port_override.is_some() || self.stored.enabled
    }

    fn port(&self) -> u16 {
        self.port_override
            .or(self.stored.port)
            .unwrap_or(DEFAULT_API_PORT)
    }
}

impl Inner {
    fn status(&self, state: &State) -> ApiStatus {
        let address = state.server.as_ref().map(ApiServer::local_addr);
        ApiStatus {
            enabled: state.enabled(),
            switchable: self.store.is_some(),
            port: address.map_or_else(|| state.port(), |address| address.port()),
            address,
            token: state.token.clone(),
            error: state.error.clone(),
        }
    }

    /// Use the stored token, or make and store one: the app's API always
    /// has one.
    async fn ensure_token(&self, state: &mut State) -> Result<()> {
        if state.token.is_some() {
            return Ok(());
        }
        if let Some(token) = state.stored.token() {
            state.token = Some(token);
            return Ok(());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("nowhere to keep a token"))?;
        let token = ApiToken::generate();
        let mut stored = state.stored.clone();
        stored.set_token(&token);
        store
            .set(&API, &stored)
            .await
            .context("couldn't save the API settings")?;
        state.stored = stored;
        state.token = Some(token);
        Ok(())
    }

    async fn listen(&self, state: &mut State) -> Result<()> {
        let port = state.port();
        if self.store.is_some() && state.token.is_none() {
            bail!("the API has no token");
        }
        let server = ApiServer::start(
            ApiServerConfig::new(port)?.with_host(self.host),
            self.core.clone(),
            state.token.clone(),
            self.shutdown.child_token(),
        )
        .await
        .with_context(|| format!("couldn’t listen on port {port}"))?;
        info!(address = %server.local_addr(), "local control API listening");
        state.server = Some(server);
        state.error = None;
        Ok(())
    }

    async fn stop(&self, state: &mut State) {
        if let Some(server) = state.server.take()
            && let Err(error) = server.shutdown().await
        {
            warn!(%error, "the HTTP API didn't stop cleanly");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing;

    async fn test_core() -> Core {
        testing::handle().await.0
    }

    #[tokio::test]
    async fn the_stored_api_starts_off_and_switches_on_with_a_kept_token() {
        let store = Store::open_in_memory().await.unwrap();
        let switch = ApiSwitch::start(
            ApiMode::Stored {
                port: None,
                token: None,
            },
            store.clone(),
            test_core().await,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = switch.status().await;
        assert!(!status.enabled && status.switchable);
        assert_eq!(status.address, None);
        assert_eq!(status.token, None, "no token until it is first on");
        assert_eq!(status.port, DEFAULT_API_PORT);

        // Not the default port, which the owner's app may hold.
        let mut stored = store.get(&API).await.unwrap().unwrap_or_default();
        stored.port = Some(free_port());
        store.set(&API, &stored).await.unwrap();
        let switch = restart(&store).await;

        let on = switch.set_enabled(true).await.unwrap();
        let address = on.address.expect("it listens");
        let token = on.token.clone().expect("it made a token");
        let stored = store.get(&API).await.unwrap().unwrap_or_default();
        assert!(stored.enabled);
        assert_eq!(stored.token(), Some(token.clone()));
        assert_eq!(get(address, None).await, 401);
        assert_eq!(get(address, Some(&token)).await, 200);

        // It comes back on, with the same token, after a restart.
        switch.shutdown().await.unwrap();
        let switch = restart(&store).await;
        let status = switch.status().await;
        assert!(status.enabled);
        assert_eq!(status.token, Some(token.clone()));
        let address = status.address.expect("it listens again");
        assert_eq!(get(address, Some(&token)).await, 200);

        let renewed = switch.new_token().await.unwrap();
        let new_token = renewed.token.clone().unwrap();
        assert_ne!(new_token, token);
        let address = renewed.address.unwrap();
        assert_eq!(get(address, Some(&token)).await, 401);
        assert_eq!(get(address, Some(&new_token)).await, 200);

        let off = switch.set_enabled(false).await.unwrap();
        assert!(!off.enabled);
        assert_eq!(off.address, None);
        let stored = store.get(&API).await.unwrap().unwrap_or_default();
        assert!(!stored.enabled);
        assert_eq!(stored.token(), Some(new_token), "the token is kept");
    }

    #[tokio::test]
    async fn a_port_in_use_leaves_it_off_and_says_why() {
        let store = Store::open_in_memory().await.unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let stored = StoredApi {
            port: Some(port),
            ..StoredApi::default()
        };
        store.set(&API, &stored).await.unwrap();
        let switch = restart(&store).await;
        assert!(switch.set_enabled(true).await.is_err());
        let status = switch.status().await;
        assert!(!status.enabled);
        assert!(!store.get(&API).await.unwrap().unwrap_or_default().enabled);

        // Stored as on, it starts anyway, with the reason.
        let mut stored = store.get(&API).await.unwrap().unwrap_or_default();
        stored.enabled = true;
        store.set(&API, &stored).await.unwrap();
        let status = restart(&store).await.status().await;
        assert!(status.enabled);
        assert_eq!(status.address, None);
        assert!(
            status.error.as_deref().unwrap().contains(&port.to_string()),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn a_port_for_the_run_turns_it_on_without_storing_that() {
        let store = Store::open_in_memory().await.unwrap();
        let token = ApiToken::from_secret("for-this-run").unwrap();
        let switch = ApiSwitch::start(
            ApiMode::Stored {
                port: Some(0),
                token: Some(token.clone()),
            },
            store.clone(),
            test_core().await,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = switch.status().await;
        assert!(status.enabled);
        assert_eq!(status.token, Some(token));
        assert_ne!(status.address.unwrap().port(), 0);
        assert!(!store.get(&API).await.unwrap().unwrap_or_default().enabled);
    }

    #[tokio::test]
    async fn an_always_on_api_cant_be_switched() {
        let store = Store::open_in_memory().await.unwrap();
        let switch = ApiSwitch::start(
            ApiMode::Always {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                token: None,
            },
            store.clone(),
            test_core().await,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = switch.status().await;
        assert!(status.enabled && !status.switchable);
        assert_eq!(get(status.address.unwrap(), None).await, 200);
        assert!(switch.set_enabled(false).await.is_err());
        assert!(switch.new_token().await.is_err());
        assert_eq!(store.get(&API).await.unwrap(), None);
    }

    async fn restart(store: &Store) -> ApiSwitch {
        ApiSwitch::start(
            ApiMode::Stored {
                port: None,
                token: None,
            },
            store.clone(),
            test_core().await,
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn get(address: SocketAddr, token: Option<&ApiToken>) -> u16 {
        let mut request = reqwest::Client::new().get(format!("http://{address}/api/v1/status"));
        if let Some(token) = token {
            request = request.bearer_auth(token.expose_secret());
        }
        request.send().await.unwrap().status().as_u16()
    }
}

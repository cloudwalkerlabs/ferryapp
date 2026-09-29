//! The composition root: builds a whole daemon (core, plugins, LAN
//! transport and control API) for the CLI and embedders, and is the one
//! place that names the built-in plugins.

mod api_switch;

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use api_switch::{ApiMode, ApiStatus, ApiSwitch};

use crate::{
    config::{LocalIdentity, default_config_dir},
    core::{Core, LocalDeviceSnapshot, Settings, SettingsDefaults, StoredSettings, TransferConfig},
    plugins::{
        self,
        clipboard::{ClipboardService, InMemoryClipboard, SystemClipboard},
    },
    protocol::{DeviceType, is_forbidden_name_character, is_valid_device_name},
    store::Store,
    transport::{
        lan::{DISCOVERY_PORT, LanConfig, LanService, LocalDeviceInfo},
        tls::subject_public_key_info,
    },
};

/// Options for starting the Ferry service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    /// How the HTTP API is served: for the whole run, or as the app's
    /// stored choice.
    pub api: ApiMode,
    /// Directory in which received files should be stored. Overrides the
    /// stored setting for this run only.
    pub download_dir: Option<PathBuf>,
    /// Directory holding the daemon's data (`ferry.db`: its identity,
    /// paired devices and settings). Defaults to
    /// the platform configuration directory.
    pub data_dir: Option<PathBuf>,
    /// Name this device advertises to peers. Overrides the stored setting
    /// for this run only; with neither, the host name is used.
    pub device_name: Option<String>,
    /// Keep discovery and every connection a device makes to this one
    /// (control and payload ports) on loopback, instead of the real
    /// network: see [`LanConfig::loopback`]. A physical switch never
    /// reflects a broadcast frame back to the port it arrived on, so two
    /// instances on the same host normally can't discover each other over a
    /// real NIC; loopback broadcast does not have that limitation. Useful
    /// for running multiple local instances against each other without a
    /// second machine; devices on the LAN can't see or reach this one.
    pub discovery_loopback: bool,
    /// UDP port loopback discovery listens and announces on, 1716 by
    /// default. Only a loopback run may change it: devices on the LAN
    /// listen on 1716 alone. On Linux, an instance that isn't on loopback
    /// (a real Ferry or KDE Connect) binds `0.0.0.0:1716`, which also
    /// receives broadcasts to `127.255.255.255:1716`, so a loopback instance
    /// on 1716 is seen and dialled by it. Another port keeps a test's or
    /// agent's instances apart from it, and from other runs'.
    pub discovery_port: u16,
    /// Sync the desktop clipboard rather than an in-memory one. Falls back to
    /// the in-memory clipboard, with a warning, when the session has no
    /// usable clipboard (e.g. no display server).
    pub system_clipboard: bool,
}

impl Default for RunRequest {
    fn default() -> Self {
        Self {
            api: ApiMode::default(),
            download_dir: None,
            data_dir: None,
            device_name: None,
            discovery_loopback: false,
            discovery_port: DISCOVERY_PORT,
            system_clipboard: false,
        }
    }
}

/// A started daemon: LAN transport, core, and control API.
///
/// The CLI runs one until Ctrl-C; the desktop app (`ferry-gui`) starts
/// one with [`RunningService::start_with`], hands its [`RunningService::core`]
/// to the UI, and shuts it down when the user quits.
pub struct RunningService {
    core: Core,
    lan: LanService,
    api: ApiSwitch,
}

impl RunningService {
    /// Start with the built-in plugins ([`plugins::builtin`]).
    pub async fn start(request: RunRequest) -> Result<Self> {
        Self::start_with(request, plugins::builtin).await
    }

    /// Start with the plugins `plugins` builds from the clipboard this run
    /// chose (the desktop's or an in-memory one). The desktop app uses this
    /// to keep each plugin's UI half next to the instance the core runs.
    pub async fn start_with(
        request: RunRequest,
        plugins: impl FnOnce(Arc<dyn ClipboardService + Send + Sync>) -> Vec<plugins::BuiltinPlugin>,
    ) -> Result<Self> {
        if !request.discovery_loopback && request.discovery_port != DISCOVERY_PORT {
            bail!(
                "discovery port {} needs loopback discovery: devices on the network listen on {DISCOVERY_PORT}",
                request.discovery_port
            );
        }
        let config_dir = request
            .data_dir
            .clone()
            .or_else(default_config_dir)
            .context("could not determine configuration directory")?;
        let store = Store::open(&config_dir).await?;
        let identity = Arc::new(LocalIdentity::load_or_create(&store).await?);
        let local_public_key_der = subject_public_key_info(identity.certificate_der())
            .context("local identity certificate could not be parsed")?;
        let settings = Settings::new(SettingsDefaults {
            device_name: default_device_name(),
            download_dir: default_download_dir().unwrap_or_else(|| config_dir.join("downloads")),
        })
        .with_store(store.clone())
        .await
        .with_overrides(StoredSettings {
            device_name: request.device_name.clone(),
            download_dir: request
                .download_dir
                .as_deref()
                .map(|directory| std::path::absolute(directory).unwrap_or(directory.into())),
            ..StoredSettings::default()
        });
        let initial = settings.snapshot();
        let mut transfer_config = TransferConfig::new(initial.download_dir.clone());
        if request.discovery_loopback {
            // Payload ports are the other thing a device dials.
            transfer_config = transfer_config.with_payload_bind_ip(Ipv4Addr::LOCALHOST);
        }
        let device_name = initial.device_name.clone();
        let system_clipboard = if request.system_clipboard {
            SystemClipboard::start()
                .inspect_err(|error| warn!(%error, "using an in-memory clipboard instead"))
                .ok()
        } else {
            None
        };
        let clipboard: Arc<dyn ClipboardService + Send + Sync> = match system_clipboard {
            Some(clipboard) => Arc::new(clipboard),
            None => InMemoryClipboard::shared(),
        };
        let (core, commands) = Core::new(
            LocalDeviceSnapshot {
                device_id: identity.device_id().to_owned(),
                device_name: device_name.clone(),
            },
            8,
            local_public_key_der,
            store.clone(),
            plugins(clipboard),
            32,
            256,
            identity.clone(),
            transfer_config,
        )
        .await?;
        core.install_settings(settings);
        core.start_plugins().await;
        let shutdown = CancellationToken::new();
        let capabilities = core.capabilities();
        let lan_config = if request.discovery_loopback {
            LanConfig::loopback(request.discovery_port)
        } else {
            LanConfig::default()
        };
        let lan = LanService::start(
            lan_config,
            LocalDeviceInfo {
                device_id: identity.device_id().to_owned(),
                device_name,
                device_type: DeviceType::Desktop,
                incoming_capabilities: capabilities.incoming,
                outgoing_capabilities: capabilities.outgoing,
            },
            core.clone(),
            commands,
            identity,
            store.clone(),
            shutdown.clone(),
        )
        .await?;
        info!(tcp_address = %lan.tcp_addr(), "LAN transport listening");

        let api = ApiSwitch::start(request.api, store, core.clone(), shutdown.clone()).await?;

        Ok(Self { core, lan, api })
    }

    /// The HTTP API: where it listens, and (in the app) turning it on and
    /// off.
    pub fn api(&self) -> &ApiSwitch {
        &self.api
    }

    /// The running core, for a frontend in the same process that reads
    /// snapshots and subscribes to events directly instead of over HTTP.
    pub fn core(&self) -> &Core {
        &self.core
    }

    /// Stop the control API and LAN transport, give in-flight transfers a
    /// bounded window to clean up their partial files, then stop the
    /// plugins.
    pub async fn shutdown(self) -> Result<()> {
        let Self { core, lan, api } = self;
        let server_result = api.shutdown().await;
        let lan_result = lan.shutdown().await;
        core.shutdown_transfers(Duration::from_secs(5)).await;
        core.shutdown_plugins().await;
        server_result?;
        lan_result?;
        Ok(())
    }
}

/// Run the daemon in the foreground until Ctrl-C.
pub async fn run_service(request: RunRequest) -> Result<()> {
    let service = RunningService::start(request).await?;
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;
    service.shutdown().await
}

/// The host name, trimmed to its first label and to what a KDE Connect
/// device name allows, or "Ferry" if nothing usable is left.
fn default_device_name() -> String {
    device_name_from_host(&gethostname::gethostname().to_string_lossy())
}

fn device_name_from_host(host: &str) -> String {
    let label = host.split('.').next().unwrap_or_default();
    let name: String = label
        .chars()
        .filter(|character| !character.is_control() && !is_forbidden_name_character(*character))
        .take(32)
        .collect();
    let name = name.trim();
    if is_valid_device_name(name) {
        name.to_owned()
    } else {
        "Ferry".to_owned()
    }
}

/// The platform download directory, if one can be determined. Falls back to
/// a `downloads` directory under the Ferry configuration directory.
fn default_download_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|dirs| dirs.download_dir().map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_loopback_discovery_takes_another_port() {
        let error = RunningService::start(RunRequest {
            discovery_port: 25123,
            ..RunRequest::default()
        })
        .await
        .err()
        .expect("a LAN run on another port is refused");
        assert!(error.to_string().contains("needs loopback discovery"));
    }

    #[test]
    fn device_names_from_host_names_fit_the_identity_schema() {
        assert_eq!(device_name_from_host("desk"), "desk");
        assert_eq!(device_name_from_host("desk.example.org"), "desk");
        assert_eq!(
            device_name_from_host("a-very-long-host-name-that-keeps-on-going"),
            "a-very-long-host-name-that-keeps"
        );
        assert_eq!(device_name_from_host("(desk)"), "desk");
        assert_eq!(device_name_from_host(""), "Ferry");
        assert_eq!(device_name_from_host(".local"), "Ferry");
    }
}

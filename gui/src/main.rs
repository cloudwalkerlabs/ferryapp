//! The Ferry desktop app: the composition root for the daemon and its
//! UI in one process. It reads the flags and runs the UI
//! (`ferry::ui`) with a way to start the daemon with every plugin; the
//! UI starts it (again on Retry) and shuts it down on exit.

// No console window behind the app on Windows, except in debug builds,
// where it shows the logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, builder::BoolishValueParser};
use ferry::{
    daemon::{ControlMode, RunRequest, RunningService},
    plugins,
    transport::lan::DISCOVERY_PORT,
    ui::{self, UiOptions},
};

/// The Ferry desktop app. Each flag can also be set through the
/// environment variable named after it.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Directory holding identity, trust and settings.
    #[arg(long, env = "FERRY_DATA_DIR", value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,
    /// Directory received files are saved to, for this run only.
    #[arg(long, env = "FERRY_DOWNLOAD_DIR", value_name = "DIRECTORY")]
    download_dir: Option<PathBuf>,
    /// Name this device advertises, for this run only.
    #[arg(long, env = "FERRY_DEVICE_NAME", value_name = "NAME")]
    device_name: Option<String>,
    /// Keep discovery and connections on loopback, off the LAN.
    #[arg(long, env = "FERRY_DISCOVERY_LOOPBACK", value_parser = BoolishValueParser::new())]
    discovery_loopback: bool,
    /// UDP port loopback discovery uses instead of 1716, to keep this
    /// instance apart from a Ferry or KDE Connect on this machine that
    /// isn't on loopback (on Linux it hears loopback announcements on 1716).
    #[arg(
        long,
        env = "FERRY_DISCOVERY_PORT",
        value_name = "PORT",
        requires = "discovery_loopback"
    )]
    discovery_port: Option<u16>,
    /// Sync an in-memory clipboard instead of the desktop's.
    #[arg(long, env = "FERRY_NO_SYSTEM_CLIPBOARD", value_parser = BoolishValueParser::new())]
    no_system_clipboard: bool,
    /// Serve the control socket the CLI uses (`ferry.sock` in the data
    /// directory) for this run, whether or not "Command line access" is on
    /// in Settings.
    #[arg(long, env = "FERRY_CLI_ACCESS", value_parser = BoolishValueParser::new())]
    cli_access: bool,
    /// Start in the tray without opening the window, as when started on
    /// login. The window opens anyway if there is no tray.
    #[arg(long)]
    background: bool,
    /// Add made-up paired devices that change over time, to see the UI
    /// without real ones.
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // iced_winit logs each new window's attributes at info,
                // the icon's pixels one byte per line.
                .unwrap_or_else(|_| "info,iced_winit=warn".into()),
        )
        .init();
    let args = Args::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("ferry")
        .build()
        .context("could not start async runtime")?;
    let data_dir = args.data_dir.clone();
    let request = RunRequest {
        control: ControlMode::Stored {
            force: args.cli_access,
        },
        data_dir: args.data_dir,
        download_dir: args.download_dir,
        device_name: args.device_name,
        discovery_loopback: args.discovery_loopback,
        discovery_port: args.discovery_port.unwrap_or(DISCOVERY_PORT),
        system_clipboard: !args.no_system_clipboard,
    };
    let start = move || -> ui::StartFuture {
        let request = request.clone();
        Box::pin(async move {
            let mut ui_plugins = None;
            let service = RunningService::start_with(request, |clipboard| {
                let parts = plugins::builtin_parts(clipboard);
                ui_plugins = Some((parts.clipboard, parts.browse, parts.notifications));
                parts.core
            })
            .await?;
            let (clipboard, browse, notifications) =
                ui_plugins.expect("the daemon built its plugins");
            let control = service.control().status().await;
            tracing::info!(
                device_id = service.core().local_device_id(),
                control_socket = control
                    .listening
                    .then(|| control.path.display().to_string()),
                "daemon started"
            );
            Ok(ui::Started {
                service,
                clipboard,
                browse,
                notifications,
            })
        })
    };

    let result = ui::run(
        UiOptions {
            runtime: runtime.handle().clone(),
            demo: args.demo,
            version: env!("FERRY_APP_VERSION").into(),
            data_dir,
            background: args.background,
        },
        start,
    );
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn arguments_are_well_formed() {
        Args::command().debug_assert();
    }
}

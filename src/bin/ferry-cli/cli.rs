use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ferry::{
    api::DEFAULT_API_PORT,
    client::{
        API_TOKEN_ENV, API_URL_ENV, ApiClient, ClipboardWatchUpdate, DeviceWatchUpdate,
        NotificationWatchUpdate, TransferWatchUpdate,
    },
    config::{ApiToken, StoredApi, default_config_dir},
    core::{
        Appearance, CoreEvent, DeviceSnapshot, EventData, PairingSnapshot, SettingsPatch,
        SettingsSnapshot, TransferSnapshot,
    },
    daemon::{ApiMode, RunRequest},
    plugins::{
        battery::BatteryStatus,
        browse::{DirectoryListing, FileEntry, FileKind},
        clipboard::{ClipboardSettings, ClipboardSnapshot},
        notifications::{Notification, NotificationPosted, NotificationRemoved},
        ping::ReceivedPing,
        share::{ReceivedShare, SharedContent},
    },
    transport::lan::DISCOVERY_PORT,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Connect and communicate with your devices.
///
/// `run` is the daemon; every other command talks to a running one over its
/// HTTP API: `ferry-cli run`'s, or the Ferry app's once "Command line
/// access" is on in its Settings. The app's is found without flags: its
/// port and token are read from its data directory.
#[derive(Debug, Parser)]
#[command(name = "ferry-cli", version, about)]
pub struct Cli {
    /// Emit newline-delimited JSON rather than human-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// Directory holding the daemon's data (its identity, paired devices
    /// and settings): the daemon's for
    /// `run`; for every other command, where to find the app's API port
    /// and token. Defaults to the platform's configuration directory.
    #[arg(long, global = true, env = "FERRY_DATA_DIR", value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,
    /// Host of the local control API: listened on by `run`, connected to by
    /// every other command. Defaults to 127.0.0.1.
    #[arg(long, global = true, value_name = "HOST")]
    api_host: Option<String>,
    /// Port of the local control API: listened on by `run`, connected to by
    /// every other command. Defaults to 24816.
    #[arg(long, global = true, value_name = "PORT")]
    api_port: Option<u16>,
    /// Bearer token for the local control API: required from clients by
    /// `run` when set, and sent by every other command. For `run`, empty
    /// (the default) disables API authentication; other commands then send
    /// the app's token, if it has one.
    #[arg(
        long,
        global = true,
        value_name = "TOKEN",
        env = API_TOKEN_ENV,
        hide_env_values = true,
        default_value = ""
    )]
    api_token: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run the Ferry daemon in the foreground.
    Run {
        #[arg(long, value_name = "DIRECTORY")]
        download_dir: Option<PathBuf>,
        /// Name this device advertises to peers.
        #[arg(long, value_name = "NAME")]
        device_name: Option<String>,
        /// Keep discovery and connections on loopback instead of the real
        /// network, so multiple local instances can discover each other
        /// without a second machine. Nothing listens on other interfaces:
        /// devices on the LAN can neither discover nor reach this one.
        #[arg(long)]
        discovery_loopback: bool,
        /// UDP port loopback discovery uses instead of 1716. On Linux a
        /// Ferry or KDE Connect on this machine that isn't on loopback hears
        /// loopback announcements on 1716 and connects; another port keeps
        /// this instance apart from it. Instances meet only on the same port.
        #[arg(long, value_name = "PORT", requires = "discovery_loopback")]
        discovery_port: Option<u16>,
        /// Sync the desktop clipboard instead of an in-memory one, which
        /// only `ferry-cli clipboard` can read and write.
        #[arg(long)]
        system_clipboard: bool,
    },
    /// List known devices.
    Devices {
        #[arg(long)]
        watch: bool,
    },
    /// Broadcast a discovery request and list unpaired devices that answer.
    Scan {
        /// Announce to this IPv4 address instead of broadcasting, for
        /// networks where broadcast doesn't reach the other device.
        #[arg(long, value_name = "IP")]
        address: Option<Ipv4Addr>,
        /// Seconds to wait for devices to respond before listing results.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
        /// Keep listening and print unpaired devices as they appear.
        #[arg(long)]
        watch: bool,
    },
    /// Start, accept, or reject pairing.
    Pair {
        #[arg(value_name = "DEVICE_ID | ACTION PAIRING_ID", num_args = 1..=2, required = true)]
        arguments: Vec<String>,
    },
    /// Unpair and forget a device.
    Unpair { device_id: String },
    /// Ping a paired device, optionally with a message.
    Ping {
        device_id: String,
        message: Option<String>,
    },
    /// Make a paired device ring so you can find it.
    Ring { device_id: String },
    /// Send text to a paired device (KDE Connect copies it to the
    /// clipboard).
    ShareText { device_id: String, text: String },
    /// Send a link to a paired device, which opens it.
    ShareUrl { device_id: String, url: String },
    /// Send a file to a paired device.
    Send {
        device_id: String,
        file: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    /// Browse a paired device's files (KDE Connect for Android shares
    /// them). Paths are absolute paths on the device.
    Files {
        device_id: String,
        #[command(subcommand)]
        action: FilesAction,
    },
    /// List, answer or dismiss a paired device's notifications (KDE
    /// Connect for Android shares them once allowed to read them).
    Notifications {
        device_id: String,
        #[command(subcommand)]
        action: Option<NotificationsAction>,
    },
    /// Read, update, or watch synchronized clipboard text.
    Clipboard {
        #[command(subcommand)]
        action: ClipboardAction,
    },
    /// Show the daemon's settings, or change the ones given.
    Settings {
        /// Name this device advertises to peers.
        #[arg(long, value_name = "NAME")]
        device_name: Option<String>,
        /// Absolute path where received files are saved.
        #[arg(long, value_name = "DIRECTORY")]
        download_dir: Option<PathBuf>,
        /// Whether to sync the clipboard with paired devices.
        #[arg(long, value_name = "BOOL")]
        clipboard_sync: Option<bool>,
        /// Whether the desktop app keeps running in the tray when its
        /// window is closed.
        #[arg(long, value_name = "BOOL")]
        close_to_tray: Option<bool>,
        /// The desktop app's language, a BCP 47 tag such as `de` or
        /// `zh-CN`, or `system` to follow the system's.
        #[arg(long, value_name = "TAG")]
        language: Option<String>,
        /// Whether the desktop app is light or dark, or `system` to follow
        /// the system's.
        #[arg(long, value_name = "MODE", value_parser = ["light", "dark", SYSTEM_APPEARANCE])]
        appearance: Option<String>,
    },
}

enum PairAction {
    Start(String),
    Accept(Uuid),
    Reject(Uuid),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum FilesAction {
    /// List a directory, or, without a path, the storage the device shares.
    Ls { path: Option<String> },
    /// Save a file into the download directory.
    Get {
        path: String,
        #[arg(long)]
        watch: bool,
    },
    /// Write a file's content to standard output.
    Cat { path: String },
    /// Upload a local file into a directory on the device.
    Put {
        file: PathBuf,
        directory: String,
        #[arg(long)]
        watch: bool,
    },
    /// Create a directory.
    Mkdir { path: String },
    /// Move or rename a file or directory. Never replaces anything.
    Mv { from: String, to: String },
    /// Delete a file, or a directory and everything in it.
    Rm { path: String },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum NotificationsAction {
    /// List them (the default).
    Ls {
        /// Keep listening and print changes as they happen.
        #[arg(long)]
        watch: bool,
    },
    /// Answer one that takes a reply.
    Reply { id: String, message: String },
    /// Press one of its buttons, by label.
    Action { id: String, action: String },
    /// Dismiss one on the device.
    Dismiss { id: String },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum ClipboardAction {
    Get,
    Set {
        text: String,
    },
    Watch,
    /// Send the clipboard text to one paired device now, e.g. one that
    /// missed an automatic sync.
    Send {
        device_id: String,
    },
}

impl Cli {
    pub async fn execute(self) -> Result<()> {
        let Self {
            json,
            data_dir,
            api_host,
            api_port,
            api_token,
            command,
        } = self;
        let api_token = (!api_token.is_empty())
            .then(|| ApiToken::from_secret(api_token))
            .transpose()
            .context("invalid --api-token")?;
        if let Command::Run {
            download_dir,
            device_name,
            discovery_loopback,
            discovery_port,
            system_clipboard,
        } = command
        {
            let host = match api_host {
                Some(host) => host
                    .parse::<IpAddr>()
                    .with_context(|| format!("invalid --api-host address: {host}"))?,
                None => IpAddr::V4(Ipv4Addr::LOCALHOST),
            };
            let request = RunRequest {
                api: ApiMode::Always {
                    host,
                    port: api_port.unwrap_or(DEFAULT_API_PORT),
                    token: api_token,
                },
                download_dir,
                data_dir,
                device_name,
                discovery_loopback,
                discovery_port: discovery_port.unwrap_or(DISCOVERY_PORT),
                system_clipboard,
            };
            return ferry::daemon::run_service(request).await;
        }

        // What the app stored, for what neither a flag nor the environment
        // gives.
        let stored = data_dir
            .or_else(default_config_dir)
            .and_then(|directory| StoredApi::read(&directory))
            .unwrap_or_default();
        let base_url_override = if api_host.is_some() || api_port.is_some() {
            let host = api_host.unwrap_or_else(|| "127.0.0.1".to_owned());
            let port = api_port.unwrap_or(DEFAULT_API_PORT);
            Some(format!("http://{}:{port}", format_host_for_url(&host)))
        } else {
            stored_api_url(&stored)
        };
        let client =
            ApiClient::from_environment_with(base_url_override, api_token.or(stored.token()))?;
        match command {
            Command::Run { .. } => unreachable!("run handled before client configuration"),
            Command::Devices { watch: false } => print_devices(&client.devices().await?, json),
            Command::Devices { watch: true } => {
                client
                    .watch_devices(cancellation_on_ctrl_c(), |update| match update {
                        DeviceWatchUpdate::Snapshot(devices) => print_devices(&devices, json),
                        DeviceWatchUpdate::Event(event) => print_event(&event, json),
                    })
                    .await?;
            }
            Command::Scan {
                address,
                timeout,
                watch,
            } => {
                client.scan(address).await?;
                if watch {
                    client
                        .watch_devices(cancellation_on_ctrl_c(), |update| match update {
                            DeviceWatchUpdate::Snapshot(devices) => {
                                print_devices(&unpaired(devices), json)
                            }
                            DeviceWatchUpdate::Event(event) if event_device_unpaired(&event) => {
                                print_event(&event, json)
                            }
                            DeviceWatchUpdate::Event(_) => {}
                        })
                        .await?;
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(timeout)).await;
                    print_devices(&unpaired(client.devices().await?), json);
                }
            }
            Command::Pair { arguments } => match parse_pair_action(&arguments)? {
                PairAction::Start(device_id) => {
                    print_pairing(&client.start_pairing(&device_id).await?, json)
                }
                PairAction::Accept(pairing_id) => {
                    print_pairing(&client.accept_pairing(pairing_id).await?, json)
                }
                PairAction::Reject(pairing_id) => {
                    client.reject_pairing(pairing_id).await?;
                    if json {
                        println!("{}", json!({"pairingId": pairing_id, "status": "rejected"}));
                    } else {
                        println!("Pairing {pairing_id} rejected");
                    }
                }
            },
            Command::Unpair { device_id } => {
                client.unpair(&device_id).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "unpaired"}));
                } else {
                    println!("Device {device_id} unpaired");
                }
            }
            Command::Ping { device_id, message } => {
                client.ping(&device_id, message.as_deref()).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "sent"}));
                } else {
                    println!("Ping sent to {device_id}");
                }
            }
            Command::Ring { device_id } => {
                client.ring(&device_id).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "sent"}));
                } else {
                    println!("Asked {device_id} to ring");
                }
            }
            Command::ShareText { device_id, text } => {
                client.share_text(&device_id, &text).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "sent"}));
                } else {
                    println!("Text sent to {device_id}");
                }
            }
            Command::ShareUrl { device_id, url } => {
                client.share_url(&device_id, &url).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "sent"}));
                } else {
                    println!("Link sent to {device_id}");
                }
            }
            Command::Send {
                device_id,
                file,
                watch,
            } => {
                let transfer = client.send_file(&device_id, &file).await?;
                if watch {
                    client
                        .watch_transfer(
                            transfer.id,
                            cancellation_on_ctrl_c(),
                            |update| match update {
                                TransferWatchUpdate::Snapshot(transfer) => {
                                    print_transfer(&transfer, json)
                                }
                                TransferWatchUpdate::Event(event) => print_event(&event, json),
                            },
                        )
                        .await?;
                } else {
                    print_transfer(&transfer, json);
                }
            }
            Command::Files { device_id, action } => {
                let watch_transfer = |transfer: TransferSnapshot, watch: bool| {
                    let client = &client;
                    async move {
                        if !watch {
                            print_transfer(&transfer, json);
                            return anyhow::Ok(());
                        }
                        client
                            .watch_transfer(transfer.id, cancellation_on_ctrl_c(), |update| {
                                match update {
                                    TransferWatchUpdate::Snapshot(transfer) => {
                                        print_transfer(&transfer, json)
                                    }
                                    TransferWatchUpdate::Event(event) => print_event(&event, json),
                                }
                            })
                            .await?;
                        Ok(())
                    }
                };
                match action {
                    FilesAction::Ls { path } => {
                        print_listing(&client.list_files(&device_id, path.as_deref()).await?, json)
                    }
                    FilesAction::Get { path, watch } => {
                        let transfer = client.download_file(&device_id, &path).await?;
                        watch_transfer(transfer, watch).await?;
                    }
                    FilesAction::Cat { path } => {
                        use std::io::Write;

                        use futures_util::StreamExt;

                        let mut content = client.file_content(&device_id, &path).await?;
                        let mut stdout = std::io::stdout().lock();
                        while let Some(chunk) = content.next().await {
                            stdout.write_all(&chunk?)?;
                        }
                        stdout.flush()?;
                    }
                    FilesAction::Put {
                        file,
                        directory,
                        watch,
                    } => {
                        let transfer = client.upload_file(&device_id, &directory, &file).await?;
                        watch_transfer(transfer, watch).await?;
                    }
                    FilesAction::Mkdir { path } => {
                        print_entry(&client.create_directory(&device_id, &path).await?, json)
                    }
                    FilesAction::Mv { from, to } => {
                        print_entry(&client.move_file(&device_id, &from, &to).await?, json)
                    }
                    FilesAction::Rm { path } => {
                        client.delete_file(&device_id, &path).await?;
                        if !json {
                            println!("Deleted {path}");
                        }
                    }
                }
            }
            Command::Notifications { device_id, action } => {
                let done = |status: &str, text: String| {
                    if json {
                        println!("{}", json!({"deviceId": device_id, "status": status}));
                    } else {
                        println!("{text}");
                    }
                };
                match action.unwrap_or(NotificationsAction::Ls { watch: false }) {
                    NotificationsAction::Ls { watch: false } => {
                        print_notifications(&client.notifications(&device_id).await?, json)
                    }
                    NotificationsAction::Ls { watch: true } => {
                        client
                            .watch_notifications(&device_id, cancellation_on_ctrl_c(), |update| {
                                match update {
                                    NotificationWatchUpdate::Snapshot(notifications) => {
                                        print_notifications(&notifications, json)
                                    }
                                    NotificationWatchUpdate::Event(event) => {
                                        print_event(&event, json)
                                    }
                                }
                            })
                            .await?
                    }
                    NotificationsAction::Reply { id, message } => {
                        client
                            .reply_to_notification(&device_id, &id, &message)
                            .await?;
                        done("sent", "Reply sent".into());
                    }
                    NotificationsAction::Action { id, action } => {
                        client
                            .run_notification_action(&device_id, &id, &action)
                            .await?;
                        done("sent", format!("Pressed {action}"));
                    }
                    NotificationsAction::Dismiss { id } => {
                        client.dismiss_notification(&device_id, &id).await?;
                        done("dismissed", "Dismissed".into());
                    }
                }
            }
            Command::Clipboard {
                action: ClipboardAction::Get,
            } => print_clipboard(&client.clipboard().await?, json),
            Command::Clipboard {
                action: ClipboardAction::Set { text },
            } => print_clipboard(&client.set_clipboard(&text).await?, json),
            Command::Clipboard {
                action: ClipboardAction::Send { device_id },
            } => {
                client.send_clipboard(&device_id).await?;
                if json {
                    println!("{}", json!({"deviceId": device_id, "status": "sent"}));
                } else {
                    println!("Clipboard sent to {device_id}");
                }
            }
            Command::Clipboard {
                action: ClipboardAction::Watch,
            } => {
                client
                    .watch_clipboard(cancellation_on_ctrl_c(), |update| match update {
                        ClipboardWatchUpdate::Snapshot(clipboard) => {
                            print_clipboard(&clipboard, json)
                        }
                        ClipboardWatchUpdate::Event(event) => print_event(&event, json),
                    })
                    .await?;
            }
            Command::Settings {
                device_name,
                download_dir,
                clipboard_sync,
                close_to_tray,
                language,
                appearance,
            } => {
                let patch = SettingsPatch {
                    device_name: device_name.map(Some),
                    download_dir: download_dir.map(Some),
                    close_to_tray: close_to_tray.map(Some),
                    language: language.map(|tag| Some(tag).filter(|tag| tag != SYSTEM_LANGUAGE)),
                    appearance: appearance.map(|mode| match mode.as_str() {
                        "light" => Some(Appearance::Light),
                        "dark" => Some(Appearance::Dark),
                        _ => None,
                    }),
                    ..clipboard_sync
                        .map(ClipboardSettings::sync_enabled_patch)
                        .unwrap_or_default()
                };
                let settings = if patch == SettingsPatch::default() {
                    client.settings().await?
                } else {
                    client.update_settings(&patch).await?
                };
                print_settings(&settings, json);
            }
        }
        Ok(())
    }
}

fn parse_pair_action(arguments: &[String]) -> Result<PairAction> {
    match arguments {
        [device_id] if !matches!(device_id.as_str(), "accept" | "reject") => {
            Ok(PairAction::Start(device_id.clone()))
        }
        [action, pairing_id] if matches!(action.as_str(), "accept" | "reject") => {
            let pairing_id = pairing_id.parse::<Uuid>()?;
            if action == "accept" {
                Ok(PairAction::Accept(pairing_id))
            } else {
                Ok(PairAction::Reject(pairing_id))
            }
        }
        _ => anyhow::bail!("usage: ferry pair <device-id> | ferry pair accept|reject <pairing-id>"),
    }
}

/// Bracket a bare IPv6 address so it forms a valid URL host, leaving IPv4
/// addresses and hostnames unchanged.
fn format_host_for_url(host: &str) -> String {
    match host.parse::<Ipv6Addr>() {
        Ok(address) => format!("[{address}]"),
        Err(_) => host.to_owned(),
    }
}

fn cancellation_on_ctrl_c() -> CancellationToken {
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    cancellation
}

fn unpaired(devices: Vec<DeviceSnapshot>) -> Vec<DeviceSnapshot> {
    devices
        .into_iter()
        .filter(|device| !device.paired)
        .collect()
}

fn event_device_unpaired(event: &CoreEvent) -> bool {
    match &event.event {
        EventData::DeviceDiscovered(device)
        | EventData::DeviceConnected(device)
        | EventData::DeviceUpdated(device)
        | EventData::DeviceDisconnected(device)
        | EventData::DeviceForgotten(device) => !device.paired,
        _ => false,
    }
}

fn print_devices(devices: &[DeviceSnapshot], json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(devices).expect("snapshot serializes")
        );
    } else if devices.is_empty() {
        println!("No devices found");
    } else {
        for device in devices {
            let trust = if device.paired { "paired" } else { "unpaired" };
            let battery = match BatteryStatus::of(device) {
                Some(battery) if battery.charging => format!("{}% charging", battery.charge),
                Some(battery) => format!("{}%", battery.charge),
                None => "-".to_owned(),
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                device.device_id,
                device.device_name,
                enum_name(device.reachability),
                trust,
                battery
            );
        }
    }
}

fn print_pairing(pairing: &PairingSnapshot, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(pairing).expect("snapshot serializes")
        );
    } else {
        println!(
            "Pairing {} with {}: {}",
            pairing.id,
            pairing.device_name,
            enum_name(pairing.status)
        );
        if let Some(code) = &pairing.verification_code {
            println!("Verification code: {code}");
        }
    }
}

fn print_transfer(transfer: &TransferSnapshot, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(transfer).expect("snapshot serializes")
        );
    } else {
        println!(
            "Transfer {}: {} ({}/{})",
            transfer.id,
            enum_name(transfer.status),
            transfer.transferred_bytes,
            transfer.total_bytes
        );
    }
}

fn print_listing(listing: &DirectoryListing, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(listing).expect("listing serializes")
        );
    } else if listing.entries.is_empty() {
        println!("Empty");
    } else if listing.path.is_none() {
        // Storage roots: the name the device gives each, and where it is.
        for entry in &listing.entries {
            println!("{}\t{}", entry.name, entry.path);
        }
    } else {
        for entry in &listing.entries {
            print_entry(entry, false);
        }
    }
}

fn print_entry(entry: &FileEntry, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(entry).expect("entry serializes")
        );
        return;
    }
    let size = entry
        .size
        .map(|size| size.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let suffix = if entry.kind == FileKind::Directory {
        "/"
    } else {
        ""
    };
    println!("{size:>12}  {}{suffix}", entry.name);
}

fn print_clipboard(clipboard: &ClipboardSnapshot, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(clipboard).expect("snapshot serializes")
        );
    } else {
        println!("{}", clipboard.text);
    }
}

/// What `settings --language` takes, and prints, for the system's language.
const SYSTEM_LANGUAGE: &str = "system";
/// What `settings --appearance` takes, and prints, for the system's.
const SYSTEM_APPEARANCE: &str = "system";

fn print_settings(settings: &SettingsSnapshot, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(settings).expect("snapshot serializes")
        );
    } else {
        println!("Device name: {}", settings.device_name);
        println!("Download directory: {}", settings.download_dir.display());
        println!(
            "Clipboard sync: {}",
            ClipboardSettings::of(settings).sync_enabled
        );
        println!("Close to tray: {}", settings.close_to_tray);
        println!(
            "Language: {}",
            settings.language.as_deref().unwrap_or(SYSTEM_LANGUAGE)
        );
        println!(
            "Appearance: {}",
            match settings.appearance {
                Some(Appearance::Light) => "light",
                Some(Appearance::Dark) => "dark",
                None => SYSTEM_APPEARANCE,
            }
        );
    }
}

fn print_event(event: &CoreEvent, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(event).expect("event serializes")
        );
    } else {
        match &event.event {
            EventData::DeviceDiscovered(device)
            | EventData::DeviceConnected(device)
            | EventData::DeviceUpdated(device)
            | EventData::DeviceDisconnected(device) => {
                println!(
                    "Device {}: {}",
                    device.device_name,
                    enum_name(device.reachability)
                )
            }
            EventData::DeviceForgotten(device) => {
                println!("Device {}: forgotten", device.device_name)
            }
            EventData::TransferStarted(transfer)
            | EventData::TransferProgress(transfer)
            | EventData::TransferCompleted(transfer)
            | EventData::TransferFailed(transfer) => print_transfer(transfer, false),
            EventData::PairingRequested(pairing) | EventData::PairingUpdated(pairing) => {
                print_pairing(pairing, false)
            }
            EventData::SettingsChanged(settings) => print_settings(settings, false),
            EventData::Plugin(event) => {
                if let Some(clipboard) = event.decode::<ClipboardSnapshot>() {
                    println!("{}", clipboard.text);
                    return;
                }
                if let Some(posted) = event.decode::<NotificationPosted>() {
                    print_notification(&posted.notification);
                    return;
                }
                if let Some(removed) = event.decode::<NotificationRemoved>() {
                    println!("Removed {}", removed.id);
                    return;
                }
                if let Some(share) = event.decode::<ReceivedShare>() {
                    match share.content {
                        SharedContent::Text { text } => {
                            println!("Text from {}: {text}", share.device_name)
                        }
                        SharedContent::Link { url } => {
                            println!("Link from {}: {url}", share.device_name)
                        }
                    }
                    return;
                }
                match event.decode::<ReceivedPing>() {
                    Some(ReceivedPing {
                        device_name,
                        message: Some(message),
                        ..
                    }) => println!("Ping from {device_name}: {message}"),
                    Some(ReceivedPing { device_name, .. }) => {
                        println!("Ping from {device_name}")
                    }
                    None => println!("{}", event.event_type()),
                }
            }
        }
    }
}

fn print_notifications(notifications: &[Notification], json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(notifications).expect("notifications serialize")
        );
        return;
    }
    if notifications.is_empty() {
        println!("No notifications");
    }
    for notification in notifications {
        print_notification(notification);
    }
}

/// `[Messages] Ana: Dinner?`, then its id and what can be done with it.
fn print_notification(notification: &Notification) {
    let heading = match (&notification.title, &notification.text) {
        (Some(title), Some(text)) => format!("{title}: {text}"),
        (Some(line), None) | (None, Some(line)) => line.clone(),
        (None, None) => String::new(),
    };
    println!("[{}] {heading}", notification.app_name);
    let mut can = Vec::new();
    if notification.repliable {
        can.push("reply".to_owned());
    }
    if notification.dismissable {
        can.push("dismiss".to_owned());
    }
    can.extend(
        notification
            .actions
            .iter()
            .map(|action| format!("action \"{action}\"")),
    );
    if can.is_empty() {
        println!("  id {}", notification.id);
    } else {
        println!("  id {} ({})", notification.id, can.join(", "));
    }
}

fn enum_name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("enum serializes")
        .as_str()
        .expect("enum is a string")
        .to_owned()
}

/// The app's API address, when it serves one and `FERRY_API_URL` doesn't
/// name another.
fn stored_api_url(stored: &StoredApi) -> Option<String> {
    (stored.enabled && std::env::var_os(API_URL_ENV).is_none()).then(|| {
        format!(
            "http://127.0.0.1:{}",
            stored.port.unwrap_or(DEFAULT_API_PORT)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-0000-0000-000000000001";

    #[test]
    fn parses_every_command() {
        let cases = [
            vec!["ferry-cli", "run", "--api-port", "25000"],
            vec!["ferry-cli", "run", "--data-dir", "/tmp/ferry"],
            vec!["ferry-cli", "--data-dir", "/tmp/ferry", "devices"],
            vec!["ferry-cli", "run", "--device-name", "My Desktop"],
            vec!["ferry-cli", "run", "--discovery-loopback"],
            vec![
                "ferry-cli",
                "run",
                "--discovery-loopback",
                "--discovery-port",
                "25123",
            ],
            vec!["ferry-cli", "run", "--system-clipboard"],
            vec![
                "ferry-cli",
                "--api-host",
                "0.0.0.0",
                "--api-port",
                "25000",
                "run",
            ],
            vec!["ferry-cli", "--api-host", "192.168.1.5", "devices"],
            vec!["ferry-cli", "--api-token", "secret", "run"],
            vec!["ferry-cli", "--api-token", "secret", "devices"],
            vec!["ferry-cli", "devices"],
            vec!["ferry-cli", "devices", "--watch"],
            vec!["ferry-cli", "scan"],
            vec!["ferry-cli", "scan", "--timeout", "5"],
            vec!["ferry-cli", "scan", "--watch"],
            vec!["ferry-cli", "scan", "--address", "192.168.1.20"],
            vec!["ferry-cli", "ping", "device-id"],
            vec!["ferry-cli", "ping", "device-id", "hello"],
            vec!["ferry-cli", "ring", "device-id"],
            vec!["ferry-cli", "share-text", "device-id", "hello there"],
            vec!["ferry-cli", "share-url", "device-id", "https://kde.org"],
            vec!["ferry-cli", "pair", "device-id"],
            vec!["ferry-cli", "pair", "accept", ID],
            vec!["ferry-cli", "pair", "reject", ID],
            vec!["ferry-cli", "unpair", "device-id"],
            vec!["ferry-cli", "send", "device-id", "photo.jpg"],
            vec!["ferry-cli", "send", "device-id", "photo.jpg", "--watch"],
            vec!["ferry-cli", "files", "device-id", "ls"],
            vec![
                "ferry-cli",
                "files",
                "device-id",
                "ls",
                "/storage/emulated/0",
            ],
            vec![
                "ferry-cli",
                "files",
                "device-id",
                "get",
                "/a/b.jpg",
                "--watch",
            ],
            vec!["ferry-cli", "files", "device-id", "cat", "/a/b.txt"],
            vec!["ferry-cli", "files", "device-id", "put", "photo.jpg", "/a"],
            vec!["ferry-cli", "files", "device-id", "mkdir", "/a/new"],
            vec!["ferry-cli", "files", "device-id", "mv", "/a/x", "/a/y"],
            vec!["ferry-cli", "files", "device-id", "rm", "/a/x"],
            vec!["ferry-cli", "clipboard", "get"],
            vec!["ferry-cli", "clipboard", "set", "hello"],
            vec!["ferry-cli", "clipboard", "watch"],
            vec!["ferry-cli", "clipboard", "send", "device-id"],
            vec!["ferry-cli", "settings"],
            vec![
                "ferry-cli",
                "settings",
                "--device-name",
                "Desk",
                "--clipboard-sync",
                "false",
            ],
            vec!["ferry-cli", "--json", "devices"],
        ];
        for arguments in cases {
            Cli::try_parse_from(&arguments)
                .unwrap_or_else(|error| panic!("failed to parse {arguments:?}: {error}"));
        }
    }

    #[test]
    fn a_discovery_port_needs_loopback_discovery() {
        assert!(Cli::try_parse_from(["ferry", "run", "--discovery-port", "25123"]).is_err());
    }

    #[test]
    fn pairing_requires_exactly_one_action() {
        assert!(Cli::try_parse_from(["ferry-cli", "pair"]).is_err());
        assert!(Cli::try_parse_from(["ferry-cli", "pair", "device", "accept", ID]).is_err());
        assert!(parse_pair_action(&["accept".into()]).is_err());
    }
}

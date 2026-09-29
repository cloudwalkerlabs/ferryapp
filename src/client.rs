//! Client for the local Ferry control API.

use std::{env, future::Future, net::Ipv4Addr, path::Path, pin::Pin, time::Duration};

use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{CONTENT_LENGTH, HeaderMap, HeaderValue},
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs::File, time::sleep};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use uuid::Uuid;

use crate::{
    api::DEFAULT_API_PORT,
    config::ApiToken,
    core::{
        CoreEvent, DeviceSnapshot, EventData, PairingSnapshot, PluginEventKind, SettingsPatch,
        SettingsSnapshot, TransferSnapshot,
    },
    plugins::{
        browse::{DirectoryListing, FileEntry},
        clipboard::ClipboardSnapshot,
        notifications::{Notification, NotificationPosted, NotificationRemoved},
        telephony::{Call, CallMissed},
    },
};

pub const API_URL_ENV: &str = "FERRY_API_URL";
pub const API_TOKEN_ENV: &str = "FERRY_API_TOKEN";

pub type EventStream = Pin<Box<dyn Stream<Item = Result<CoreEvent, ClientError>> + Send + 'static>>;
pub type ByteStream =
    Pin<Box<dyn Stream<Item = Result<bytes::Bytes, ClientError>> + Send + 'static>>;

/// How long a request may wait for the next bytes from the daemon.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ApiClient {
    base_url: Url,
    token: Option<ApiToken>,
    http: Client,
    /// For streaming uploads: [`ApiClient::http`] without its read timeout.
    /// The daemon answers an upload only once the whole file has been
    /// forwarded, so a read timeout would fail every upload that takes
    /// longer than it. The daemon ends an upload that stalls (`408`).
    upload_http: Client,
}

impl ApiClient {
    pub fn from_environment() -> Result<Self, ClientError> {
        Self::from_environment_with(None, None)
    }

    /// Like [`ApiClient::from_environment`], but explicit values (e.g. from
    /// `--api-host`/`--api-port`/`--api-token` flags) take precedence over the
    /// `FERRY_API_URL` and `FERRY_API_TOKEN` environment variables.
    /// Without a token from either source, requests are sent unauthenticated.
    pub fn from_environment_with(
        base_url_override: Option<String>,
        token_override: Option<ApiToken>,
    ) -> Result<Self, ClientError> {
        let base_url = base_url_override.unwrap_or_else(|| {
            env::var(API_URL_ENV).unwrap_or_else(|_| format!("http://127.0.0.1:{DEFAULT_API_PORT}"))
        });
        let token = match token_override {
            Some(token) => Some(token),
            None => match env::var(API_TOKEN_ENV) {
                Ok(secret) if secret.is_empty() => None,
                Ok(secret) => Some(ApiToken::from_secret(secret)?),
                Err(env::VarError::NotPresent) => None,
                Err(env::VarError::NotUnicode(_)) => return Err(ClientError::InvalidToken),
            },
        };
        Self::new(&base_url, token)
    }

    pub fn new(base_url: &str, token: Option<ApiToken>) -> Result<Self, ClientError> {
        Self::with_read_timeout(base_url, token, READ_TIMEOUT)
    }

    fn with_read_timeout(
        base_url: &str,
        token: Option<ApiToken>,
        read_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let mut base_url = Url::parse(base_url).map_err(|_| ClientError::InvalidApiUrl)?;
        if base_url.scheme() != "http" {
            return Err(ClientError::UnsupportedScheme);
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        Ok(Self {
            base_url,
            token,
            http: Client::builder()
                .connect_timeout(Duration::from_secs(3))
                // The server emits SSE keepalives every 15 seconds, so a read
                // deadline detects a dead daemon without imposing a total
                // lifetime on watch connections.
                .read_timeout(read_timeout)
                .build()
                .map_err(ClientError::Build)?,
            upload_http: Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()
                .map_err(ClientError::Build)?,
        })
    }

    pub async fn devices(&self) -> Result<Vec<DeviceSnapshot>, ClientError> {
        self.get_json("api/v1/devices", "device").await
    }

    /// Trigger an immediate discovery broadcast, so newly reachable devices
    /// show up in [`ApiClient::devices`] without waiting for the periodic
    /// announce interval. With an `address`, announce to that IPv4 address
    /// only, for networks where broadcast doesn't reach the peer.
    pub async fn scan(&self, address: Option<Ipv4Addr>) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Discovery {
            address: String,
        }

        let mut request = self.authorized(self.http.post(self.url("api/v1/discovery")?));
        if let Some(address) = address {
            request = request.json(&Discovery {
                address: address.to_string(),
            });
        }
        let response = request.send().await.map_err(map_transport)?;
        checked(response, "discovery").await?;
        Ok(())
    }

    /// Wait for a device to answer at `address` (up to ten seconds) and
    /// return it, ready to pair. A device that pairs afterwards keeps the
    /// address, to be reached at when broadcast doesn't find it. Fails with
    /// `address_unreachable` when nothing answers.
    pub async fn connect(&self, address: Ipv4Addr) -> Result<DeviceSnapshot, ClientError> {
        #[derive(Serialize)]
        struct Connect {
            address: String,
        }

        let response = self
            .authorized(self.http.post(self.url("api/v1/devices/connect")?))
            .json(&Connect {
                address: address.to_string(),
            })
            .send()
            .await
            .map_err(map_transport)?;
        let response = checked(response, "device").await?;
        response.json().await.map_err(map_transport)
    }

    /// Replace the addresses a paired device is reached at when broadcast
    /// doesn't find it.
    pub async fn set_device_addresses(
        &self,
        device_id: &str,
        addresses: &[Ipv4Addr],
    ) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Addresses {
            addresses: Vec<String>,
        }

        let response = self
            .authorized(
                self.http
                    .put(self.url(&format!("api/v1/devices/{device_id}/addresses"))?),
            )
            .json(&Addresses {
                addresses: addresses.iter().map(ToString::to_string).collect(),
            })
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    pub async fn start_pairing(&self, device_id: &str) -> Result<PairingSnapshot, ClientError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct StartPairing<'a> {
            device_id: &'a str,
        }

        let response = self
            .authorized(self.http.post(self.url("api/v1/pairings")?))
            .json(&StartPairing { device_id })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "pairing").await
    }

    pub async fn pairing(&self, pairing_id: Uuid) -> Result<PairingSnapshot, ClientError> {
        self.get_json(&format!("api/v1/pairings/{pairing_id}"), "pairing")
            .await
    }

    pub async fn accept_pairing(&self, pairing_id: Uuid) -> Result<PairingSnapshot, ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/pairings/{pairing_id}/accept"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "pairing").await
    }

    pub async fn reject_pairing(&self, pairing_id: Uuid) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .delete(self.url(&format!("api/v1/pairings/{pairing_id}"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "pairing").await?;
        Ok(())
    }

    pub async fn unpair(&self, device_id: &str) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .delete(self.url(&format!("api/v1/devices/{device_id}"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Ping a paired, connected device, optionally attaching a message.
    pub async fn ping(&self, device_id: &str, message: Option<&str>) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Ping<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            message: Option<&'a str>,
        }

        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/ping"))?),
            )
            .json(&Ping { message })
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Ask a paired, connected device to ring so it can be found.
    pub async fn ring(&self, device_id: &str) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/ring"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Send this machine's clipboard text to a paired, connected device.
    pub async fn send_clipboard(&self, device_id: &str) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/clipboard"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Send text to a paired, connected device.
    pub async fn share_text(&self, device_id: &str, text: &str) -> Result<(), ClientError> {
        self.share_content(device_id, "text", &serde_json::json!({ "text": text }))
            .await
    }

    /// Send a link to a paired, connected device, which opens it.
    pub async fn share_url(&self, device_id: &str, url: &str) -> Result<(), ClientError> {
        self.share_content(device_id, "url", &serde_json::json!({ "url": url }))
            .await
    }

    async fn share_content(
        &self,
        device_id: &str,
        kind: &str,
        body: &serde_json::Value,
    ) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/share/{kind}"))?),
            )
            .json(body)
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Send a file to a paired, connected device. The response comes once
    /// the whole file has been handed to the daemon, not when the device
    /// has it; follow the transfer to see it finish.
    pub async fn send_file(
        &self,
        device_id: &str,
        path: &Path,
    ) -> Result<TransferSnapshot, ClientError> {
        let (file_name, part) = file_part(path).await?;
        let form = Form::new().part("file", part.file_name(file_name));
        let response = self
            .authorized(
                self.upload_http
                    .post(self.url(&format!("api/v1/devices/{device_id}/share"))?),
            )
            .multipart(form)
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "transfer").await
    }

    /// List a directory on a paired device, or, without `path`, the
    /// storage roots it shares.
    pub async fn list_files(
        &self,
        device_id: &str,
        path: Option<&str>,
    ) -> Result<DirectoryListing, ClientError> {
        let mut url = self.url(&format!("api/v1/devices/{device_id}/files"))?;
        if let Some(path) = path {
            url.query_pairs_mut().append_pair("path", path);
        }
        let response = self
            .authorized(self.http.get(url))
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "device or directory").await
    }

    /// Stream a file's content from a paired device.
    pub async fn file_content(
        &self,
        device_id: &str,
        path: &str,
    ) -> Result<ByteStream, ClientError> {
        let mut url = self.url(&format!("api/v1/devices/{device_id}/files/content"))?;
        url.query_pairs_mut().append_pair("path", path);
        let response = self
            .authorized(self.http.get(url))
            .send()
            .await
            .map_err(map_transport)?;
        let response = checked(response, "device or file").await?;
        Ok(Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(map_transport)),
        ))
    }

    /// Save a file from a paired device into the daemon's download
    /// directory, as an incoming transfer.
    pub async fn download_file(
        &self,
        device_id: &str,
        path: &str,
    ) -> Result<TransferSnapshot, ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/files/download"))?),
            )
            .json(&FilePath { path })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "device or file").await
    }

    /// Upload a local file into `directory` on a paired device. Completes
    /// once the whole file has been forwarded.
    pub async fn upload_file(
        &self,
        device_id: &str,
        directory: &str,
        path: &Path,
    ) -> Result<TransferSnapshot, ClientError> {
        let (file_name, part) = file_part(path).await?;
        let form = Form::new()
            .text("path", directory.to_owned())
            .part("file", part.file_name(file_name));
        let response = self
            .authorized(
                self.upload_http
                    .post(self.url(&format!("api/v1/devices/{device_id}/files/upload"))?),
            )
            .multipart(form)
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "device or directory").await
    }

    /// Create a directory on a paired device.
    pub async fn create_directory(
        &self,
        device_id: &str,
        path: &str,
    ) -> Result<FileEntry, ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/files/directories"))?),
            )
            .json(&FilePath { path })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "device or directory").await
    }

    /// Move or rename a file or directory on a paired device.
    pub async fn move_file(
        &self,
        device_id: &str,
        from: &str,
        to: &str,
    ) -> Result<FileEntry, ClientError> {
        #[derive(Serialize)]
        struct Move<'a> {
            from: &'a str,
            to: &'a str,
        }

        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/files/move"))?),
            )
            .json(&Move { from, to })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "device or file").await
    }

    /// Delete a file, or a directory and everything in it, on a paired
    /// device.
    pub async fn delete_file(&self, device_id: &str, path: &str) -> Result<(), ClientError> {
        let mut url = self.url(&format!("api/v1/devices/{device_id}/files"))?;
        url.query_pairs_mut().append_pair("path", path);
        let response = self
            .authorized(self.http.delete(url))
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device or file").await?;
        Ok(())
    }

    /// The notifications a paired device shows, newest first.
    pub async fn notifications(&self, device_id: &str) -> Result<Vec<Notification>, ClientError> {
        self.get_json(
            &format!("api/v1/devices/{device_id}/notifications"),
            "device",
        )
        .await
    }

    /// Answer a notification on a paired device that takes a reply.
    pub async fn reply_to_notification(
        &self,
        device_id: &str,
        id: &str,
        message: &str,
    ) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Reply<'a> {
            id: &'a str,
            message: &'a str,
        }

        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/notifications/reply"))?),
            )
            .json(&Reply { id, message })
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device or notification").await?;
        Ok(())
    }

    /// Press one of a notification's buttons, by its label.
    pub async fn run_notification_action(
        &self,
        device_id: &str,
        id: &str,
        action: &str,
    ) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Action<'a> {
            id: &'a str,
            action: &'a str,
        }

        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/notifications/action"))?),
            )
            .json(&Action { id, action })
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device or notification").await?;
        Ok(())
    }

    /// Dismiss a notification on a paired device.
    pub async fn dismiss_notification(&self, device_id: &str, id: &str) -> Result<(), ClientError> {
        let mut url = self.url(&format!("api/v1/devices/{device_id}/notifications"))?;
        url.query_pairs_mut().append_pair("id", id);
        let response = self
            .authorized(self.http.delete(url))
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device or notification").await?;
        Ok(())
    }

    /// Turn showing a paired device's notifications here on or off. The
    /// device's snapshot says which, under `plugins.notifications`.
    pub async fn set_notifications_enabled(
        &self,
        device_id: &str,
        enabled: bool,
    ) -> Result<(), ClientError> {
        #[derive(Serialize)]
        struct Enabled {
            enabled: bool,
        }

        let response = self
            .authorized(
                self.http
                    .put(self.url(&format!("api/v1/devices/{device_id}/notifications/enabled"))?),
            )
            .json(&Enabled { enabled })
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Like [`ApiClient::watch_devices`], for one device's notifications:
    /// the list, then each `notification.*` event about the device.
    pub async fn watch_notifications<F>(
        &self,
        device_id: &str,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(NotificationWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.notifications(device_id),
            |update| {
                match update {
                    Watched::Snapshot(notifications) => {
                        on_update(NotificationWatchUpdate::Snapshot(notifications));
                    }
                    Watched::Event(event) => {
                        let EventData::Plugin(plugin_event) = &event.event else {
                            return false;
                        };
                        let about_device = plugin_event
                            .decode::<NotificationPosted>()
                            .map(|posted| posted.device_id)
                            .or_else(|| {
                                plugin_event
                                    .decode::<NotificationRemoved>()
                                    .map(|removed| removed.device_id)
                            })
                            .is_some_and(|id| id == device_id);
                        if about_device {
                            on_update(NotificationWatchUpdate::Event(event));
                        }
                    }
                }
                false
            },
        )
        .await
    }

    /// The call going on on a paired device, if any.
    pub async fn call(&self, device_id: &str) -> Result<Option<Call>, ClientError> {
        self.get_json(&format!("api/v1/devices/{device_id}/call"), "device")
            .await
    }

    /// Ask a paired phone to mute its ringer for the call ringing now.
    pub async fn mute_ringer(&self, device_id: &str) -> Result<(), ClientError> {
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("api/v1/devices/{device_id}/call/mute"))?),
            )
            .send()
            .await
            .map_err(map_transport)?;
        checked(response, "device").await?;
        Ok(())
    }

    /// Like [`ApiClient::watch_devices`], for one device's calls: the call
    /// going on, again each time it changes, and each missed call.
    pub async fn watch_call<F>(
        &self,
        device_id: &str,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(CallWatchUpdate) + Send,
    {
        // Device events also carry other changes (a battery report); only
        // a change of call is news.
        let mut last = None;
        self.watch(
            cancellation,
            || self.call(device_id),
            |update| {
                let call = match update {
                    Watched::Snapshot(call) => call,
                    Watched::Event(event) => match &event.event {
                        EventData::DeviceConnected(device)
                        | EventData::DeviceUpdated(device)
                        | EventData::DeviceDisconnected(device)
                        | EventData::DeviceForgotten(device)
                            if device.device_id == device_id =>
                        {
                            Call::of(device)
                        }
                        EventData::Plugin(event) => {
                            if let Some(missed) = event
                                .decode::<CallMissed>()
                                .filter(|missed| missed.device_id == device_id)
                            {
                                on_update(CallWatchUpdate::Missed(missed));
                            }
                            return false;
                        }
                        _ => return false,
                    },
                };
                if last.as_ref() != Some(&call) {
                    last = Some(call.clone());
                    on_update(CallWatchUpdate::Call(call));
                }
                false
            },
        )
        .await
    }

    pub async fn transfer(&self, transfer_id: Uuid) -> Result<TransferSnapshot, ClientError> {
        self.get_json(&format!("api/v1/transfers/{transfer_id}"), "transfer")
            .await
    }

    pub async fn clipboard(&self) -> Result<ClipboardSnapshot, ClientError> {
        self.get_json("api/v1/clipboard", "clipboard").await
    }

    pub async fn set_clipboard(&self, text: &str) -> Result<ClipboardSnapshot, ClientError> {
        #[derive(Serialize)]
        struct SetClipboard<'a> {
            text: &'a str,
        }

        let response = self
            .authorized(self.http.put(self.url("api/v1/clipboard")?))
            .json(&SetClipboard { text })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "clipboard").await
    }

    /// Turn clipboard sync with paired devices on or off.
    pub async fn set_clipboard_sync(
        &self,
        enabled: bool,
    ) -> Result<ClipboardSnapshot, ClientError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct PatchClipboard {
            sync_enabled: bool,
        }

        let response = self
            .authorized(self.http.patch(self.url("api/v1/clipboard")?))
            .json(&PatchClipboard {
                sync_enabled: enabled,
            })
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "clipboard").await
    }

    pub async fn settings(&self) -> Result<SettingsSnapshot, ClientError> {
        self.get_json("api/v1/settings", "settings").await
    }

    pub async fn update_settings(
        &self,
        patch: &SettingsPatch,
    ) -> Result<SettingsSnapshot, ClientError> {
        let response = self
            .authorized(self.http.patch(self.url("api/v1/settings")?))
            .json(patch)
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, "settings").await
    }

    pub async fn events(&self) -> Result<EventStream, ClientError> {
        let response = self
            .authorized(self.http.get(self.url("api/v1/events")?))
            .send()
            .await
            .map_err(map_transport)?;
        let response = checked(response, "event stream").await?;
        let mut chunks = response.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut pending = Vec::new();
            while let Some(chunk) = chunks.next().await {
                pending.extend_from_slice(&chunk.map_err(map_transport)?);
                while let Some((end, delimiter_length)) = find_sse_frame(&pending) {
                    let frame: Vec<u8> = pending.drain(..end + delimiter_length).collect();
                    if let Some(event) = decode_sse_frame(&frame)? {
                        yield event;
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }

    /// Print-ready updates for the device list: a snapshot, then device
    /// events, and a fresh snapshot after every reconnect. Runs until
    /// `cancellation`.
    pub async fn watch_devices<F>(
        &self,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(DeviceWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.devices(),
            |update| {
                match update {
                    Watched::Snapshot(devices) => on_update(DeviceWatchUpdate::Snapshot(devices)),
                    Watched::Event(event) if is_device_event(&event.event) => {
                        on_update(DeviceWatchUpdate::Event(event));
                    }
                    Watched::Event(_) => {}
                }
                false
            },
        )
        .await
    }

    /// Like [`ApiClient::watch_devices`], for the synced clipboard text.
    pub async fn watch_clipboard<F>(
        &self,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(ClipboardWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.clipboard(),
            |update| {
                match update {
                    Watched::Snapshot(clipboard) => {
                        on_update(ClipboardWatchUpdate::Snapshot(clipboard));
                    }
                    Watched::Event(event)
                        if event.event.event_type() == ClipboardSnapshot::TYPE =>
                    {
                        on_update(ClipboardWatchUpdate::Event(event));
                    }
                    Watched::Event(_) => {}
                }
                false
            },
        )
        .await
    }

    /// Like [`ApiClient::watch_devices`], for one transfer, ending once the
    /// transfer has (or at once if it already had).
    pub async fn watch_transfer<F>(
        &self,
        transfer_id: Uuid,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(TransferWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.transfer(transfer_id),
            |update| match update {
                Watched::Snapshot(transfer) => {
                    let terminal = transfer.status.is_terminal();
                    on_update(TransferWatchUpdate::Snapshot(transfer));
                    terminal
                }
                Watched::Event(event) => {
                    let Some(transfer) = transfer_from_event(&event.event)
                        .filter(|transfer| transfer.id == transfer_id)
                    else {
                        return false;
                    };
                    let terminal = transfer.status.is_terminal();
                    on_update(TransferWatchUpdate::Event(event));
                    terminal
                }
            },
        )
        .await
    }

    /// Follow a resource: its snapshot from `load`, then every event, and
    /// again after the event stream drops, until `on_update` returns `true`
    /// or `cancellation`. Subscribes to `/events` before loading the
    /// snapshot, so a change made in between arrives as an event instead of
    /// being missed; the daemon subscribes before it answers.
    async fn watch<S, Load, Loading, F>(
        &self,
        cancellation: CancellationToken,
        load: Load,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        Load: Fn() -> Loading,
        Loading: Future<Output = Result<S, ClientError>>,
        F: FnMut(Watched<S>) -> bool,
    {
        loop {
            let mut events = self.events().await?;
            if on_update(Watched::Snapshot(load().await?)) || cancellation.is_cancelled() {
                return Ok(());
            }
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    event = events.next() => match event {
                        Some(Ok(event)) => {
                            if on_update(Watched::Event(event)) {
                                return Ok(());
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = sleep(Duration::from_millis(250)) => {}
            }
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        resource: &'static str,
    ) -> Result<T, ClientError> {
        let response = self
            .authorized(self.http.get(self.url(path)?))
            .send()
            .await
            .map_err(map_transport)?;
        decode_json(response, resource).await
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token.expose_secret()),
            None => request,
        }
    }

    fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base_url
            .join(path)
            .map_err(|_| ClientError::InvalidApiUrl)
    }
}

#[derive(Serialize)]
struct FilePath<'a> {
    path: &'a str,
}

/// A multipart part streaming the file at `path`, and the file's name.
async fn file_part(path: &Path) -> Result<(String, Part), ClientError> {
    let file = File::open(path)
        .await
        .map_err(|source| ClientError::OpenFile {
            path: path.to_owned(),
            source,
        })?;
    let length = file
        .metadata()
        .await
        .map_err(|source| ClientError::OpenFile {
            path: path.to_owned(),
            source,
        })?
        .len();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ClientError::InvalidFileName)?
        .to_owned();
    let stream = ReaderStream::new(file);
    // The daemon checks the declared size before streaming, and reads it
    // from the part's own Content-Length header, which
    // `stream_with_length` does not set.
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    let part = Part::stream_with_length(reqwest::Body::wrap_stream(stream), length)
        .headers(headers)
        .mime_str("application/octet-stream")
        .map_err(ClientError::Build)?;
    Ok((file_name, part))
}

pub enum DeviceWatchUpdate {
    Snapshot(Vec<DeviceSnapshot>),
    Event(CoreEvent),
}

pub enum NotificationWatchUpdate {
    Snapshot(Vec<Notification>),
    Event(CoreEvent),
}

pub enum CallWatchUpdate {
    /// The call going on now, or `None`.
    Call(Option<Call>),
    Missed(CallMissed),
}

#[derive(Debug)]
pub enum ClipboardWatchUpdate {
    Snapshot(ClipboardSnapshot),
    Event(CoreEvent),
}

pub enum TransferWatchUpdate {
    Snapshot(TransferSnapshot),
    Event(CoreEvent),
}

/// What [`ApiClient::watch`] hands its callback.
enum Watched<S> {
    Snapshot(S),
    Event(CoreEvent),
}

fn is_device_event(event: &EventData) -> bool {
    matches!(
        event,
        EventData::DeviceDiscovered(_)
            | EventData::DeviceConnected(_)
            | EventData::DeviceUpdated(_)
            | EventData::DeviceDisconnected(_)
            | EventData::DeviceForgotten(_)
    )
}

fn transfer_from_event(event: &EventData) -> Option<&TransferSnapshot> {
    match event {
        EventData::TransferStarted(transfer)
        | EventData::TransferProgress(transfer)
        | EventData::TransferCompleted(transfer)
        | EventData::TransferFailed(transfer) => Some(transfer),
        _ => None,
    }
}

fn find_sse_frame(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(frame), None) | (None, Some(frame)) => Some(frame),
        (None, None) => None,
    }
}

fn decode_sse_frame(frame: &[u8]) -> Result<Option<CoreEvent>, ClientError> {
    let mut data = Vec::new();
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| ClientError::InvalidEvent)
}

async fn decode_json<T: for<'de> Deserialize<'de>>(
    response: Response,
    resource: &'static str,
) -> Result<T, ClientError> {
    checked(response, resource)
        .await?
        .json()
        .await
        .map_err(|_| ClientError::InvalidResponse)
}

async fn checked(response: Response, resource: &'static str) -> Result<Response, ClientError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(ClientError::Unauthorized);
    }
    if status == StatusCode::NOT_FOUND {
        return Err(ClientError::NotFound(resource));
    }
    let code = response
        .json::<Problem>()
        .await
        .ok()
        .map(|problem| problem.code)
        .unwrap_or_else(|| "unknown_error".to_owned());
    Err(ClientError::OperationFailed {
        status: status.as_u16(),
        code,
    })
}

fn map_transport(error: reqwest::Error) -> ClientError {
    if error.is_connect() || error.is_timeout() {
        ClientError::DaemonUnavailable
    } else {
        ClientError::Transport(error)
    }
}

#[derive(Deserialize)]
struct Problem {
    code: String,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(
        "the Ferry daemon is unavailable; start it with `ferry-cli run`, or turn on \
         Command line access in the Ferry app's Settings"
    )]
    DaemonUnavailable,
    #[error(
        "the daemon requires a valid API token; pass --api-token or set {API_TOKEN_ENV} \
         (the Ferry app's Settings show its token under Command line access)"
    )]
    Unauthorized,
    #[error("the requested {0} was not found")]
    NotFound(&'static str),
    #[error("the daemon could not complete the operation ({status}: {code})")]
    OperationFailed { status: u16, code: String },
    #[error("the daemon returned an invalid response")]
    InvalidResponse,
    #[error("the daemon returned an invalid event")]
    InvalidEvent,
    #[error("the API URL is invalid")]
    InvalidApiUrl,
    #[error("the API URL must use HTTP")]
    UnsupportedScheme,
    #[error("the API token is invalid")]
    InvalidToken,
    #[error("could not open {path}")]
    OpenFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the file name is not valid Unicode")]
    InvalidFileName,
    #[error("could not construct the HTTP client request")]
    Build(#[source] reqwest::Error),
    #[error("local API request failed")]
    Transport(#[source] reqwest::Error),
    #[error("the API token is invalid")]
    Token(#[from] crate::config::ApiTokenError),
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    /// An upload that takes longer than the read timeout still gets its
    /// answer: the daemon says nothing until the whole file is in.
    #[tokio::test]
    async fn an_upload_outlasts_the_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Longer than the client's read timeout, as a slow upload is.
            sleep(Duration::from_millis(600)).await;
            let mut buffer = [0; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"hello").unwrap();
        let client = ApiClient::with_read_timeout(
            &format!("http://{address}"),
            None,
            Duration::from_millis(200),
        )
        .unwrap();

        let result = client.send_file("peer", file.path()).await;

        assert!(
            matches!(result, Err(ClientError::NotFound(_))),
            "unexpected result: {result:?}"
        );
        server.await.unwrap();
    }
}

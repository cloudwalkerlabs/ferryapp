//! Helpers for the UI's tests.

use std::{fs, path::Path};

use iced::{Element, Settings, Size, Task, futures::StreamExt};
use iced_test::simulator::Simulator;

use crate::{
    config::LocalIdentity,
    core::{Core, DeviceReachability, DeviceSnapshot, SettingsSnapshot, testing::make_identity},
    protocol::{DeviceType, Packet, PairingBody},
    ui::{
        i18n,
        store::{Snapshot, Store},
        widgets,
    },
};

/// A paired, connected phone named `name`, with no capabilities.
pub fn device(name: &str) -> DeviceSnapshot {
    DeviceSnapshot {
        device_id: format!("{:0<32}", name.replace(' ', "")),
        device_name: name.into(),
        device_type: DeviceType::Phone,
        protocol_version: 8,
        incoming_capabilities: vec![],
        outgoing_capabilities: vec![],
        reachability: DeviceReachability::Connected,
        paired: true,
        pairing: false,
        last_seen_at: 0,
        plugins: Default::default(),
    }
}

/// A valid device id for [`connect_peer`].
pub const PEER_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";

/// A peer named "Peer" that `core` trusts and is connected to, receiving
/// `incoming_capabilities`. What the core sends it arrives on the returned
/// receiver; the connection lasts as long as the receiver.
pub fn connect_peer(
    core: &Core,
    device_id: &str,
    incoming_capabilities: &[&str],
) -> (DeviceSnapshot, tokio::sync::mpsc::Receiver<Packet>) {
    let capabilities = incoming_capabilities.iter().map(|c| (*c).into()).collect();
    core.discover_device(&make_identity(device_id, capabilities), true, 1)
        .expect("discovered");
    let (packets, received) = tokio::sync::mpsc::channel(8);
    let device = core
        .register_connection(
            device_id,
            vec![1, 2, 3],
            8,
            packets,
            tokio_util::sync::CancellationToken::new(),
            1,
        )
        .expect("connected");
    (device, received)
}

/// A peer named "Peer" that `core` is connected to but doesn't trust. It
/// has a real certificate, so it can be paired. What the core sends it
/// arrives on the returned receiver.
pub fn connect_unpaired_peer(
    core: &Core,
    device_id: &str,
) -> (DeviceSnapshot, tokio::sync::mpsc::Receiver<Packet>) {
    core.discover_device(&make_identity(device_id, Vec::new()), false, 1)
        .expect("discovered");
    let store = crate::store::Store::open_in_memory().expect("a store");
    let identity = LocalIdentity::load_or_create(&store).expect("an identity");
    let (packets, received) = tokio::sync::mpsc::channel(8);
    let device = core
        .register_connection(
            device_id,
            identity.certificate_der().to_vec(),
            8,
            packets,
            tokio_util::sync::CancellationToken::new(),
            1,
        )
        .expect("connected");
    (device, received)
}

/// The peer `device_id` asks `core` to pair, as a KDE Connect device does.
pub fn request_pairing(core: &Core, device_id: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let body = PairingBody {
        pair: true,
        timestamp: Some(now.try_into().expect("a sane clock")),
        extra: Default::default(),
    };
    core.handle_peer_packet(
        device_id,
        Packet::from_body(1, "kdeconnect.pair", &body).expect("a packet"),
    );
}

/// A store holding `devices`, on a computer named `local_name`, with no
/// pairings or transfers.
pub fn store(local_name: &str, devices: Vec<DeviceSnapshot>) -> Store {
    let mut store = Store::default();
    store.apply_snapshot(Snapshot {
        devices: Ok(devices),
        pairings: Ok(Vec::new()),
        transfers: Vec::new(),
        settings: Ok(SettingsSnapshot {
            device_name: local_name.into(),
            download_dir: "/home/me/Downloads".into(),
            close_to_tray: true,
            language: None,
            appearance: None,
            plugins: Default::default(),
        }),
    });
    store
}

/// Run `task` to the end and return what it produced. Only for tasks that
/// just produce values: window and widget actions are dropped.
pub async fn outputs<T: 'static>(task: Task<T>) -> Vec<T> {
    let Some(stream) = iced_runtime::task::into_stream(task) else {
        return Vec::new();
    };
    stream
        .filter_map(async |action| match action {
            iced_runtime::Action::Output(output) => Some(output),
            _ => None,
        })
        .collect()
        .await
}

/// Render `view` headlessly, in light and dark, to
/// `$SNAPSHOT_DIR/<name>-<light|dark>-<backend>.png`, and in the en-XA
/// pseudo-locale, light only, to `<name>-en-XA-light-<backend>.png`, to
/// look at the UI without a display. Does nothing without the variable.
/// `SNAPSHOT_LANGUAGES`, a comma-separated list such as `de,zh-CN`, adds
/// those translations, light only, as `<name>-<language>-light-…`.
///
/// In en-XA, a string that reads as plain English wasn't extracted (or
/// was made before `view` ran, like a toast's text), and a missing closing
/// bracket means the text was cut off.
pub fn snapshot<'a, Message>(
    name: &str,
    size: impl Into<Size> + Copy,
    view: impl Fn() -> Element<'a, Message>,
) {
    let Ok(directory) = std::env::var("SNAPSHOT_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    let languages = std::env::var("SNAPSHOT_LANGUAGES").unwrap_or_default();
    let variants = [
        ("light".to_owned(), super::theme::light(), None),
        ("dark".to_owned(), super::theme::dark(), None),
    ]
    .into_iter()
    .chain(
        std::iter::once("en-XA")
            .chain(languages.split(',').map(str::trim))
            .filter(|language| !language.is_empty())
            .map(|language| {
                (
                    format!("{language}-light"),
                    super::theme::light(),
                    Some(language),
                )
            }),
    );
    for (variant, theme, language) in variants {
        let stem = format!("{name}-{variant}");
        remove_old_images(directory, &stem);
        let settings = Settings {
            fonts: std::iter::once(iced_fonts::LUCIDE_FONT_BYTES)
                .chain(widgets::FONT_FACES)
                .map(Into::into)
                .collect(),
            default_font: widgets::FONT,
            ..Settings::default()
        };
        // Through layout too: `responsive` builds its content then.
        let render = || {
            let mut ui = Simulator::with_size(settings, size, view());
            ui.snapshot(&theme).expect("snapshot renders")
        };
        let snapshot = match language {
            Some(language) => i18n::in_locale(language, render),
            None => render(),
        };
        assert!(
            snapshot
                .matches_image(directory.join(&stem))
                .expect("snapshot saves")
        );
    }
}

/// `matches_image` compares with an existing image instead of writing a new
/// one, and adds the renderer's name to the file name.
fn remove_old_images(directory: &Path, stem: &str) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let prefix = format!("{stem}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".png") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

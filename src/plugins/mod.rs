//! The daemon's features.
//!
//! Each feature implements [`crate::core::Plugin`] and is listed in
//! [`builtin`]: ping, find my phone, battery, connectivity, clipboard,
//! share, browse, notifications and telephony.
//! The set is fixed at compile time; nothing is loaded at runtime. A plugin
//! reaches the core through its [`crate::core::PluginContext`], never another
//! plugin. See `docs/ARCHITECTURE.md` §2.

pub mod battery;
pub mod browse;
pub mod clipboard;
pub mod connectivity;
pub mod findmyphone;
pub mod notifications;
pub mod ping;
pub mod share;
pub mod telephony;

use std::sync::Arc;

use crate::core::{DeviceSnapshot, Plugin, PluginContext};
use crate::protocol::Packet;
use crate::rpc::Methods;
use futures_util::future::BoxFuture;
use serde_json::Value;

/// Lists every plugin once and forwards the [`Plugin`] contract to the
/// concrete one: no erased objects, so callbacks are plain awaited futures.
macro_rules! builtin_plugins {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        /// One of this build's plugins, sharing its instance with the UI.
        #[derive(Clone)]
        pub enum BuiltinPlugin {
            $($variant(Arc<$ty>)),*
        }

        $(impl From<Arc<$ty>> for BuiltinPlugin {
            fn from(plugin: Arc<$ty>) -> Self { Self::$variant(plugin) }
        })*

        impl BuiltinPlugin {
            pub fn id(&self) -> &'static str {
                match self { $(Self::$variant(p) => p.id()),* }
            }
            pub fn incoming(&self) -> &'static [&'static str] {
                match self { $(Self::$variant(p) => p.incoming()),* }
            }
            pub fn outgoing(&self) -> &'static [&'static str] {
                match self { $(Self::$variant(p) => p.outgoing()),* }
            }
            pub async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
                match self { $(Self::$variant(p) => p.handle_packet(ctx, device, packet).await),* }
            }
            pub fn methods(&self, ctx: PluginContext, methods: &mut Methods) {
                match self { $(Self::$variant(p) => p.clone().methods(ctx, methods)),* }
            }
            pub fn device_state(&self, ctx: &PluginContext, device: &DeviceSnapshot) -> Option<Value> {
                match self { $(Self::$variant(p) => p.device_state(ctx, device)),* }
            }
            pub async fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
                match self { $(Self::$variant(p) => p.connected(ctx, device).await),* }
            }
            pub async fn paired(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
                match self { $(Self::$variant(p) => p.paired(ctx, device).await),* }
            }
            pub async fn disconnected(&self, ctx: &PluginContext, device_id: &str) {
                match self { $(Self::$variant(p) => p.disconnected(ctx, device_id).await),* }
            }
            pub async fn unpaired(&self, ctx: &PluginContext, device_id: &str) {
                match self { $(Self::$variant(p) => p.unpaired(ctx, device_id).await),* }
            }
            pub async fn started(&self, ctx: &PluginContext) {
                match self { $(Self::$variant(p) => p.clone().started(ctx).await),* }
            }
            pub fn shutdown(&self) -> BoxFuture<'_, ()> {
                match self { $(Self::$variant(p) => p.shutdown()),* }
            }
        }
    };
}

builtin_plugins! {
    Ping(ping::PingPlugin),
    FindMyPhone(findmyphone::FindMyPhonePlugin),
    Battery(battery::BatteryPlugin),
    Connectivity(connectivity::ConnectivityPlugin),
    Clipboard(clipboard::ClipboardPlugin),
    Share(share::SharePlugin),
    Browse(browse::BrowsePlugin),
    Notifications(notifications::NotificationsPlugin),
    Telephony(telephony::TelephonyPlugin),
}

/// Every plugin in this build. `clipboard` is the clipboard that clipboard
/// sync reads and writes: the desktop's, or an in-memory one.
pub fn builtin(
    clipboard: Arc<dyn clipboard::ClipboardService + Send + Sync>,
) -> Vec<BuiltinPlugin> {
    builtin_parts(clipboard).core
}

/// The plugins [`builtin_parts`] builds: the core's list, and the
/// instances in it that the desktop app's UI calls too.
pub struct Parts {
    pub core: Vec<BuiltinPlugin>,
    pub clipboard: Arc<clipboard::ClipboardPlugin>,
    pub browse: Arc<browse::BrowsePlugin>,
    pub notifications: Arc<notifications::NotificationsPlugin>,
}

/// Every plugin in this build, and the instances among them that the UI
/// calls too.
pub fn builtin_parts(clipboard: Arc<dyn clipboard::ClipboardService + Send + Sync>) -> Parts {
    let clipboard = Arc::new(clipboard::ClipboardPlugin::new(clipboard));
    let browse = Arc::new(browse::BrowsePlugin::default());
    let notifications = Arc::new(notifications::NotificationsPlugin::default());
    Parts {
        core: vec![
            Arc::new(ping::PingPlugin).into(),
            Arc::new(findmyphone::FindMyPhonePlugin).into(),
            Arc::new(battery::BatteryPlugin::default()).into(),
            Arc::new(connectivity::ConnectivityPlugin::default()).into(),
            clipboard.clone().into(),
            Arc::new(share::SharePlugin).into(),
            browse.clone().into(),
            notifications.clone().into(),
            Arc::new(telephony::TelephonyPlugin::default()).into(),
        ],
        clipboard,
        browse,
        notifications,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::PluginRegistry;

    #[test]
    fn parts_share_their_instances_with_the_core_list() {
        let parts = builtin_parts(clipboard::InMemoryClipboard::shared());
        let mut shared = 0;
        for plugin in &parts.core {
            match plugin {
                BuiltinPlugin::Clipboard(plugin) => {
                    assert!(Arc::ptr_eq(plugin, &parts.clipboard));
                    shared += 1;
                }
                BuiltinPlugin::Browse(plugin) => {
                    assert!(Arc::ptr_eq(plugin, &parts.browse));
                    shared += 1;
                }
                BuiltinPlugin::Notifications(plugin) => {
                    assert!(Arc::ptr_eq(plugin, &parts.notifications));
                    shared += 1;
                }
                _ => {}
            }
        }
        assert_eq!(shared, 3);
    }

    #[test]
    fn advertises_ping_clipboard_and_share_both_directions_and_the_rest_one_way() {
        // Order doesn't matter to peers, so compare sorted lists.
        fn sorted(mut values: Vec<String>) -> Vec<String> {
            values.sort();
            values
        }
        fn strings(values: &[&str]) -> Vec<String> {
            sorted(values.iter().map(|value| value.to_string()).collect())
        }
        let capabilities =
            PluginRegistry::new(builtin(clipboard::InMemoryClipboard::shared())).capabilities();
        let bidirectional = [
            ping::PACKET_TYPE,
            clipboard::PACKET_TYPE,
            clipboard::CONNECT_PACKET_TYPE,
            share::PACKET_TYPE,
        ];
        assert_eq!(
            sorted(capabilities.incoming),
            strings(
                &[
                    &bidirectional[..],
                    &[
                        browse::PACKET_TYPE,
                        battery::PACKET_TYPE,
                        connectivity::PACKET_TYPE,
                        notifications::PACKET_TYPE,
                        telephony::PACKET_TYPE,
                    ]
                ]
                .concat()
            )
        );
        assert_eq!(
            sorted(capabilities.outgoing),
            strings(
                &[
                    &bidirectional[..],
                    &[
                        browse::REQUEST_PACKET_TYPE,
                        findmyphone::REQUEST_PACKET_TYPE,
                        notifications::REQUEST_PACKET_TYPE,
                        notifications::REPLY_PACKET_TYPE,
                        notifications::ACTION_PACKET_TYPE,
                        telephony::MUTE_PACKET_TYPE,
                    ]
                ]
                .concat()
            )
        );
    }
}

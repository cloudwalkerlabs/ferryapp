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

use crate::core::Plugin;

/// Every plugin in this build. `clipboard` is the clipboard that clipboard
/// sync reads and writes: the desktop's, or an in-memory one.
pub fn builtin(
    clipboard: Arc<dyn clipboard::ClipboardService + Send + Sync>,
) -> Vec<Arc<dyn Plugin>> {
    builtin_parts(clipboard).core
}

/// The plugins [`builtin_parts`] builds: the core's list, and the
/// instances in it that the desktop app's UI calls too.
pub struct Parts {
    pub core: Vec<Arc<dyn Plugin>>,
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
            Arc::new(ping::PingPlugin),
            Arc::new(findmyphone::FindMyPhonePlugin),
            Arc::new(battery::BatteryPlugin::default()),
            Arc::new(connectivity::ConnectivityPlugin::default()),
            clipboard.clone(),
            Arc::new(share::SharePlugin),
            browse.clone(),
            notifications.clone(),
            Arc::new(telephony::TelephonyPlugin::default()),
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

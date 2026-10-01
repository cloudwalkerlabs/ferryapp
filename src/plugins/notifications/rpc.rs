//! Notifications' control methods. A notification's id is the device's
//! own and may hold any character (Android's keys have `|`).

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};

use super::{Notification, NotificationError, NotificationsPlugin};
use crate::{
    core::PluginContext,
    rpc::{ErrorCode, Methods, RpcError, define_methods},
};

define_methods! {
    /// The device's notifications, as this machine shows them.
    "notifications.list" => ListNotifications { device_id: String } -> Vec<Notification>;
    /// Turn showing the device's notifications here on or off. Its snapshot
    /// says which under `plugins.notifications`.
    "notifications.setEnabled" => SetNotificationsEnabled { device_id: String, enabled: bool } -> ();
    /// Dismiss one on the device.
    "notifications.dismiss" => DismissNotification { device_id: String, id: String } -> ();
    /// Answer one that takes a reply.
    "notifications.reply" => ReplyToNotification { device_id: String, id: String, message: String } -> ();
    /// Press one of its buttons, by label.
    "notifications.action" => RunNotificationAction { device_id: String, id: String, action: String } -> ();
    /// Its icon, as base64 PNG; `icon_not_found` if it has none (yet).
    "notifications.icon" => NotificationIcon { device_id: String, id: String } -> String;
}

impl ErrorCode for NotificationError {
    fn code(&self) -> &'static str {
        NotificationError::code(self)
    }
}

pub(super) fn add(plugin: Arc<NotificationsPlugin>, ctx: PluginContext, methods: &mut Methods) {
    let state = || (plugin.clone(), ctx.clone());
    methods.add(
        state(),
        |(plugin, ctx), ListNotifications { device_id }| async move {
            plugin.notifications(&ctx, &device_id)
        },
    );
    methods.add(
        state(),
        |(plugin, ctx), SetNotificationsEnabled { device_id, enabled }| async move {
            plugin.set_enabled(&ctx, &device_id, enabled).await
        },
    );
    methods.add(
        state(),
        |(plugin, ctx), DismissNotification { device_id, id }| async move {
            plugin.dismiss(&ctx, &device_id, &id)
        },
    );
    methods.add(
        state(),
        |(plugin, ctx),
         ReplyToNotification {
             device_id,
             id,
             message,
         }| async move { plugin.reply(&ctx, &device_id, &id, &message) },
    );
    methods.add(
        state(),
        |(plugin, ctx),
         RunNotificationAction {
             device_id,
             id,
             action,
         }| async move { plugin.run_action(&ctx, &device_id, &id, &action) },
    );
    methods.add(
        state(),
        |(plugin, ctx), NotificationIcon { device_id, id }| async move {
            plugin.notifications(&ctx, &device_id)?;
            let icon = plugin.icon(&device_id, &id).ok_or_else(|| {
                RpcError::failed("icon_not_found", "the notification has no icon")
            })?;
            Ok::<_, RpcError>(STANDARD.encode(icon))
        },
    );
}

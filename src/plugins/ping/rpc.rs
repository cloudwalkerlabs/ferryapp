//! Ping's control methods.

use super::send_ping;
use crate::{
    core::PluginContext,
    rpc::{Methods, define_methods},
};

define_methods! {
    /// Queue a `kdeconnect.ping`, with `message` if given, to a paired,
    /// connected device.
    "ping.send" => Ping { device_id: String, #[serde(default)] message: Option<String> } -> ();
}

pub(super) fn add(ctx: PluginContext, methods: &mut Methods) {
    methods.add(ctx, |ctx, Ping { device_id, message }| async move {
        send_ping(&ctx, &device_id, message)
    });
}

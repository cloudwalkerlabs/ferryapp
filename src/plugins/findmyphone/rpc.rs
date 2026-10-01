//! Find my phone's control methods.

use super::ring_device;
use crate::{
    core::PluginContext,
    rpc::{Methods, define_methods},
};

define_methods! {
    /// Ask a paired, connected device that advertises
    /// `kdeconnect.findmyphone.request` to ring so it can be found.
    "findmyphone.ring" => Ring { device_id: String } -> ();
}

pub(super) fn add(ctx: PluginContext, methods: &mut Methods) {
    methods.add(ctx, |ctx, Ring { device_id }| async move {
        ring_device(&ctx, &device_id)
    });
}

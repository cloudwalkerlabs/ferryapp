//! Telephony's control methods.

use super::{Call, TelephonyError, current_call, mute_ringer};
use crate::{
    core::PluginContext,
    rpc::{ErrorCode, Methods, define_methods},
};

define_methods! {
    /// The call going on on the device, or `null`.
    "telephony.call" => GetCall { device_id: String } -> Option<Call>;
    /// Ask the device to mute its ringer for the call ringing now.
    "telephony.mute" => Mute { device_id: String } -> ();
}

impl ErrorCode for TelephonyError {
    fn code(&self) -> &'static str {
        TelephonyError::code(self)
    }
}

pub(super) fn add(ctx: PluginContext, methods: &mut Methods) {
    methods.add(ctx.clone(), |ctx, GetCall { device_id }| async move {
        current_call(&ctx, &device_id)
    });
    methods.add(ctx, |ctx, Mute { device_id }| async move {
        mute_ringer(&ctx, &device_id)
    });
}

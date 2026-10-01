//! Clipboard's control methods.

use std::sync::Arc;

use super::{ClipboardPlugin, ClipboardSnapshot, ClipboardSyncError};
use crate::{
    core::PluginContext,
    rpc::{ErrorCode, Methods, define_methods},
};

define_methods! {
    /// The synced clipboard text, and whether sync is on.
    "clipboard.get" => GetClipboard {} -> ClipboardSnapshot;
    /// Set this machine's clipboard text, which syncs to paired devices
    /// while sync is on.
    "clipboard.set" => SetClipboard { text: String } -> ClipboardSnapshot;
    /// Turn clipboard sync with paired devices on or off.
    "clipboard.setSync" => SetClipboardSync { enabled: bool } -> ClipboardSnapshot;
    /// Send this machine's clipboard text to a paired, connected device,
    /// for when automatic sync missed it.
    "clipboard.send" => SendClipboard { device_id: String } -> ();
}

impl ErrorCode for ClipboardSyncError {
    fn code(&self) -> &'static str {
        ClipboardSyncError::code(self)
    }
}

pub(super) fn add(plugin: Arc<ClipboardPlugin>, ctx: PluginContext, methods: &mut Methods) {
    let state = || (plugin.clone(), ctx.clone());
    methods.add(state(), |(plugin, ctx), GetClipboard {}| async move {
        Ok::<_, ClipboardSyncError>(plugin.snapshot(&ctx))
    });
    methods.add(state(), |(plugin, ctx), SetClipboard { text }| async move {
        plugin.set_text(&ctx, text)
    });
    methods.add(
        state(),
        |(plugin, ctx), SetClipboardSync { enabled }| async move {
            plugin.set_sync_enabled(&ctx, enabled).await
        },
    );
    methods.add(state(), |(plugin, ctx), SendClipboard { device_id }| async move {
        plugin.send_to(&ctx, &device_id)
    });
}

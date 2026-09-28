//! What a feature gets from the shell while it handles a message or event:
//! the core, the store, and a way to run work on the daemon's runtime.

use std::future::Future;

use iced::Task;

use crate::{
    core::{Core, DeviceSnapshot, PluginContext, TransferSnapshot},
    ui::store::Store,
};

pub struct UiContext {
    core: Core,
    runtime: tokio::runtime::Handle,
    store: Store,
    window_focused: bool,
}

impl UiContext {
    pub(crate) fn new(core: Core, runtime: tokio::runtime::Handle) -> Self {
        Self {
            core,
            runtime,
            store: Store::default(),
            window_focused: true,
        }
    }

    pub fn core(&self) -> &Core {
        &self.core
    }

    /// What the core owns, as the UI last heard.
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// The core as a plugin's own module sees it, to call its typed API.
    pub fn plugin_context(&self) -> PluginContext {
        self.core.plugin_context()
    }

    /// Run `future` on the daemon's runtime, where the core's sockets live
    /// (iced's executor has no tokio reactor), and send `then` of its
    /// result.
    ///
    /// Write `future` as an `async move` block: some tokio futures, such as
    /// `tokio::time::sleep`, need the runtime already when they are made,
    /// and `update` doesn't run on it.
    pub fn spawn<T, M>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
        then: impl FnOnce(T) -> M + Send + 'static,
    ) -> Task<M>
    where
        T: Send + 'static,
        M: Send + 'static,
    {
        // `map` takes an `FnMut`, but the future produces one output.
        let mut then = Some(then);
        on_runtime(&self.runtime, future).map(move |output| {
            let then = then.take().expect("a future has one output");
            then(output)
        })
    }

    /// A device, from the store.
    pub fn device(&self, device_id: &str) -> Option<&DeviceSnapshot> {
        self.store.device(device_id)
    }

    /// File transfers from the store, newest first: all of them, or only
    /// those with one device. Empty until they have loaded.
    pub fn transfers(&self, device_id: Option<&str>) -> Vec<&TransferSnapshot> {
        self.store
            .transfers(device_id)
            .into_loaded()
            .unwrap_or_default()
    }

    pub fn window_focused(&self) -> bool {
        self.window_focused
    }

    pub(crate) fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }
}

/// Run `future` on `runtime` as an iced task. Produces nothing if the task
/// panics or the runtime shuts down first.
pub(crate) fn on_runtime<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> Task<T> {
    Task::future(runtime.spawn(future)).then(|result| match result {
        Ok(output) => Task::done(output),
        Err(error) => {
            tracing::warn!(%error, "UI task did not finish");
            Task::none()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::testing::handle, ui::testing};

    #[tokio::test]
    async fn spawned_work_runs_on_the_daemon_runtime() {
        let (core, _commands) = handle().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let task = ctx.spawn(
            async {
                // Needs a tokio reactor: this would panic on iced's executor.
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                7
            },
            |seven| seven * 6,
        );
        assert_eq!(testing::outputs(task).await, [42]);
    }
}

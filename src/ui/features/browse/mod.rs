//! Browse's UI: the *Browse files* action and the file browser, over the
//! shared [`BrowsePlugin`]'s methods.
//!
//! The page is [`Route::Browse`], which names the open folder, so Back, the
//! drop target and the shell all see which folder shows. A device's files
//! have no events: nothing tells the daemon when they change on the device
//! (ADR 0008). A folder is listed when it opens, again after every change
//! made here, on Refresh, and when the device can share its files again
//! after it couldn't (it reconnected).

mod describe;
pub mod files;
mod preview;
mod view;

use std::{collections::HashMap, ops::Range, path::PathBuf, sync::Arc};

use iced::{Subscription, Task, keyboard, widget::image};
use iced_fonts::lucide;

use super::{DeviceAction, DropTarget, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot},
    plugins::browse::{
        BrowsePlugin, DirectoryListing, FileEntry, FileKind, REQUEST_PACKET_TYPE,
        files::{join_remote_path, split_remote_path},
    },
    ui::{
        self, Origin,
        context::UiContext,
        error::{FileBatch, describe_file_failures},
        i18n::fl,
        route::Route,
        shell,
    },
};
pub use describe::describe_error;
use describe::describe_upload_error;
pub use files::{Answer, Files};
use preview::{Preview, can_preview};
use view::folder_name;

/// Where the browser is: a device, and a folder of it or its storage.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    device_id: String,
    folder: Option<String>,
}

pub struct BrowseUi {
    files: Arc<dyn Files>,
    /// Where the window is, while it shows the browser's page.
    open: Option<Place>,
    /// Whether the open device could share its files when last seen.
    shares: bool,
    /// The open device's folders as last listed (`None`: its storage).
    listings: HashMap<Option<String>, Listing>,
    /// Counts listing requests, so only a folder's newest answer counts.
    requests: u64,
    show_hidden: bool,
    sort: Sort,
    /// The file whose actions show under its row, by path.
    menu: Option<String>,
    preview: Option<Preview>,
}

#[derive(Default)]
struct Listing {
    /// The request whose answer is awaited or held.
    request: u64,
    /// The last answer; kept on show while a new one is on its way.
    answer: Option<Result<DirectoryListing, String>>,
}

/// A column of the listing to sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Column {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sort {
    by: Column,
    ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            by: Column::Name,
            ascending: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Open a device's files, at its storage.
    Browse {
        device_id: String,
    },
    /// Back to the device.
    Back,
    /// Show a folder of the open device, or its storage (`None`).
    Open(Option<String>),
    /// Up a folder; from a storage root, to the storage.
    Up,
    Listed {
        device_id: String,
        folder: Option<String>,
        request: u64,
        result: Result<DirectoryListing, String>,
    },
    Refresh,
    ToggleHidden,
    Sort(Column),
    /// Show or hide a file's actions.
    Menu(String),
    /// A click on a file: a folder opens, a small image previews, anything
    /// else downloads.
    Activate(FileEntry),
    Download(FileEntry),
    /// A download started, of the file of this name, or why it didn't.
    Downloading(Result<String, String>),
    Preview(FileEntry),
    Previewed {
        path: String,
        result: Result<image::Handle, String>,
    },
    ClosePreview,
    NewFolder,
    Rename(FileEntry),
    Delete(FileEntry),
    /// Change something in `folder` on the device, then list it again.
    Change {
        device_id: String,
        folder: String,
        change: Change,
    },
    /// A change or upload in `folder` finished, or failed with this
    /// sentence.
    Changed {
        device_id: String,
        folder: String,
        error: Option<String>,
    },
    /// Pick files to upload to the open folder.
    PickUploads,
    /// Upload these files into `folder`, one at a time.
    Upload {
        device_id: String,
        folder: String,
        paths: Vec<PathBuf>,
    },
}

/// A change to a device's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    CreateDirectory(String),
    Move { from: String, to: String },
    Delete(String),
}

/// Listed for every device, enabled while it shares its files.
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    vec![DeviceAction {
        id: "browse-files",
        label: fl!("browse-action"),
        icon: lucide::folder_open,
        enabled: shares_files(device),
        // The tray lists it only for a device that can share files.
        visible_in_tray: advertises_browse(device),
        message: Feature::Browse(Message::Browse {
            device_id: device.device_id.clone(),
        }),
    }]
}

/// `message` as the app's, from `origin`.
fn to_app(message: Message, origin: Origin) -> ui::Message {
    ui::Message::Feature(Feature::Browse(message), origin)
}

impl BrowseUi {
    /// The browser over `plugin`, the instance the core runs.
    pub fn new(plugin: Arc<BrowsePlugin>) -> Self {
        Self::over(plugin)
    }

    fn over(files: Arc<dyn Files>) -> Self {
        Self {
            files,
            open: None,
            shares: false,
            listings: HashMap::new(),
            requests: 0,
            show_hidden: false,
            sort: Sort::default(),
            menu: None,
            preview: None,
        }
    }

    /// List `folder` of the open device again, keeping what it held on show
    /// until the answer comes.
    fn fetch(
        &mut self,
        ctx: &UiContext,
        folder: Option<String>,
        origin: Origin,
    ) -> Task<ui::Message> {
        let Some(open) = &self.open else {
            return Task::none();
        };
        self.requests += 1;
        let request = self.requests;
        self.listings.entry(folder.clone()).or_default().request = request;
        let device_id = open.device_id.clone();
        let listed =
            self.files
                .clone()
                .list(ctx.plugin_context(), device_id.clone(), folder.clone());
        ctx.spawn(
            async move { listed.await.map_err(|error| describe_error(&error)) },
            move |result| {
                to_app(
                    Message::Listed {
                        device_id,
                        folder,
                        request,
                        result,
                    },
                    origin,
                )
            },
        )
    }

    /// List the open folder, and the storage too when it isn't held, for
    /// the breadcrumbs.
    fn fetch_open(&mut self, ctx: &UiContext) -> Task<ui::Message> {
        let Some(folder) = self.open.as_ref().map(|open| open.folder.clone()) else {
            return Task::none();
        };
        let roots_held = matches!(
            self.listings.get(&None),
            Some(Listing {
                answer: Some(Ok(_)),
                ..
            })
        );
        let mut tasks = vec![self.fetch(ctx, folder.clone(), Origin::Window)];
        if folder.is_some() && !roots_held {
            tasks.push(self.fetch(ctx, None, Origin::Window));
        }
        Task::batch(tasks)
    }

    fn open_device(&self) -> Option<String> {
        self.open.as_ref().map(|open| open.device_id.clone())
    }

    fn open_folder(&self) -> Option<&str> {
        self.open.as_ref()?.folder.as_deref()
    }

    fn roots(&self) -> &[FileEntry] {
        match self.listings.get(&None) {
            Some(Listing {
                answer: Some(Ok(listing)),
                ..
            }) => &listing.entries,
            _ => &[],
        }
    }

    fn navigate(&self, folder: Option<&str>, origin: Origin) -> Task<ui::Message> {
        match self.open_device() {
            Some(device) => shell::navigate(
                origin,
                Route::Browse {
                    device,
                    folder: folder.map(str::to_owned),
                },
            ),
            None => Task::none(),
        }
    }

    /// Ask for a name, then make `change` of it.
    fn prompt(
        &self,
        title: String,
        confirm_label: String,
        initial: &str,
        folder: String,
        origin: Origin,
        change: impl Fn(String) -> Option<Change> + Send + Sync + 'static,
    ) -> Task<ui::Message> {
        let Some(device_id) = self.open_device() else {
            return Task::none();
        };
        // Select the name without its extension, as file managers do.
        let stem = match initial.rfind('.') {
            Some(dot) if dot > 0 => initial[..dot].chars().count(),
            _ => initial.chars().count(),
        };
        shell::prompt(shell::Prompt {
            title,
            body: None,
            label: fl!("browse-name-label"),
            initial: initial.into(),
            selection: (!initial.is_empty()).then_some(Range {
                start: 0,
                end: stem,
            }),
            confirm_label,
            validate: Arc::new(invalid_name_reason),
            then: Arc::new(move |name| {
                Feature::Browse(match change(name) {
                    Some(change) => Message::Change {
                        device_id: device_id.clone(),
                        folder: folder.clone(),
                        change,
                    },
                    // Nothing to do; refetching is harmless.
                    None => Message::Changed {
                        device_id: device_id.clone(),
                        folder: folder.clone(),
                        error: None,
                    },
                })
            }),
            origin,
        })
    }

    fn change(
        &self,
        ctx: &UiContext,
        device_id: String,
        folder: String,
        change: Change,
        origin: Origin,
    ) -> Task<ui::Message> {
        let files = self.files.clone();
        let plugin_ctx = ctx.plugin_context();
        let changed = {
            let device_id = device_id.clone();
            async move {
                let result = match change {
                    Change::CreateDirectory(path) => files
                        .create_directory(plugin_ctx, device_id, path)
                        .await
                        .map(drop),
                    Change::Move { from, to } => files
                        .move_file(plugin_ctx, device_id, from, to)
                        .await
                        .map(drop),
                    Change::Delete(path) => files.delete(plugin_ctx, device_id, path).await,
                };
                result.err().map(|error| describe_error(&error))
            }
        };
        ctx.spawn(changed, move |error| {
            to_app(
                Message::Changed {
                    device_id,
                    folder,
                    error,
                },
                origin,
            )
        })
    }

    fn download(&self, ctx: &UiContext, file: FileEntry, origin: Origin) -> Task<ui::Message> {
        let Some(device_id) = self.open_device() else {
            return Task::none();
        };
        let started =
            self.files
                .clone()
                .download(ctx.plugin_context(), device_id, file.path.clone());
        ctx.spawn(
            async move {
                started
                    .await
                    .map(|_| file.name)
                    .map_err(|error| describe_error(&error))
            },
            move |result| to_app(Message::Downloading(result), origin),
        )
    }

    fn upload(
        &self,
        ctx: &UiContext,
        device_id: String,
        folder: String,
        paths: Vec<PathBuf>,
        origin: Origin,
    ) -> Task<ui::Message> {
        let files = self.files.clone();
        let plugin_ctx = ctx.plugin_context();
        let uploaded = {
            let device_id = device_id.clone();
            let folder = folder.clone();
            async move {
                let mut failures = Vec::new();
                for path in paths {
                    let upload = files.clone().upload(
                        plugin_ctx.clone(),
                        device_id.clone(),
                        folder.clone(),
                        path.clone(),
                    );
                    if let Err(error) = upload.await {
                        failures.push((path, describe_upload_error(&error)));
                    }
                }
                describe_file_failures(FileBatch::Upload, &failures)
            }
        };
        ctx.spawn(uploaded, move |error| {
            to_app(
                Message::Changed {
                    device_id,
                    folder,
                    error,
                },
                origin,
            )
        })
    }

    /// Files dropped on an open folder are uploaded into it.
    pub fn drop_target(&self, device: &DeviceSnapshot, route: &Route) -> Option<DropTarget> {
        let Route::Browse {
            device: device_id,
            folder: Some(folder),
        } = route
        else {
            return None;
        };
        if *device_id != device.device_id || !shares_files(device) {
            return None;
        }
        let device_id = device_id.clone();
        let folder = folder.clone();
        Some(DropTarget {
            label: fl!(
                "browse-drop-hint",
                folder = folder_name(&folder, self.roots())
            ),
            on_drop: Arc::new(move |paths| {
                Feature::Browse(Message::Upload {
                    device_id: device_id.clone(),
                    folder: folder.clone(),
                    paths,
                })
            }),
        })
    }

    /// The window now shows `route`. The browser loads what its page needs
    /// when the route is its own, and lets go of it otherwise.
    pub(crate) fn on_route(&mut self, ctx: &UiContext, route: &Route) -> Task<ui::Message> {
        let Route::Browse { device, folder } = route else {
            // Let go of the device's files, as the Flutter page did.
            self.open = None;
            self.listings.clear();
            self.menu = None;
            self.preview = None;
            return Task::none();
        };
        if self.open_device().as_deref() != Some(device.as_str()) {
            self.listings.clear();
            self.show_hidden = false;
            self.sort = Sort::default();
            self.preview = None;
        }
        self.menu = None;
        self.shares = ctx.device(device).is_some_and(shares_files);
        self.open = Some(Place {
            device_id: device.clone(),
            folder: folder.clone(),
        });
        if self.shares {
            self.fetch_open(ctx)
        } else {
            Task::none()
        }
    }

    /// When the open device can share its files again (it reconnected),
    /// list them again.
    pub(crate) fn on_event(&mut self, ctx: &UiContext, _event: &CoreEvent) -> Task<ui::Message> {
        let Some(device_id) = self.open_device() else {
            return Task::none();
        };
        let shares = ctx.device(&device_id).is_some_and(shares_files);
        if shares == self.shares {
            return Task::none();
        }
        self.shares = shares;
        if !shares {
            return Task::none();
        }
        let folder = self.open_folder().map(str::to_owned);
        let mut tasks = vec![self.fetch(ctx, None, Origin::Window)];
        if folder.is_some() {
            tasks.push(self.fetch(ctx, folder, Origin::Window));
        }
        Task::batch(tasks)
    }

    pub(crate) fn update(
        &mut self,
        ctx: &UiContext,
        message: Message,
        origin: Origin,
    ) -> Task<ui::Message> {
        match message {
            Message::Browse { device_id } => shell::navigate(
                origin,
                Route::Browse {
                    device: device_id,
                    folder: None,
                },
            ),
            Message::Back => match self.open_device() {
                Some(device_id) => shell::navigate(origin, Route::Device(device_id)),
                None => Task::none(),
            },
            Message::Open(folder) => self.navigate(folder.as_deref(), origin),
            Message::Up => {
                let Some(folder) = self.open_folder() else {
                    return Task::none();
                };
                let up = if self.roots().iter().any(|root| root.path == folder) {
                    None
                } else {
                    parent_of(folder)
                };
                self.navigate(up.as_deref(), origin)
            }
            Message::Listed {
                device_id,
                folder,
                request,
                result,
            } => {
                if self.open_device().as_deref() == Some(device_id.as_str())
                    && let Some(listing) = self.listings.get_mut(&folder)
                    && listing.request == request
                {
                    listing.answer = Some(result);
                }
                Task::none()
            }
            Message::Refresh => {
                let folder = self.open_folder().map(str::to_owned);
                self.fetch(ctx, folder, origin)
            }
            Message::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                Task::none()
            }
            Message::Sort(by) => {
                self.sort = Sort {
                    by,
                    ascending: self.sort.by != by || !self.sort.ascending,
                };
                Task::none()
            }
            Message::Menu(path) => {
                self.menu = (self.menu.as_ref() != Some(&path)).then_some(path);
                Task::none()
            }
            Message::Activate(file) => {
                self.menu = None;
                if file.kind == FileKind::Directory {
                    self.navigate(Some(&file.path), origin)
                } else if can_preview(&file) {
                    self.preview(ctx, file, origin)
                } else {
                    self.download(ctx, file, origin)
                }
            }
            Message::Download(file) => {
                self.menu = None;
                self.download(ctx, file, origin)
            }
            Message::Downloading(Ok(name)) => Task::done(ui::Message::Toast {
                text: fl!("browse-downloading", file = name),
                action: Some((fl!("browse-see-transfers"), Route::Transfers)),
                origin,
            }),
            Message::Downloading(Err(error)) => shell::toast(origin, error),
            Message::Preview(file) => {
                self.menu = None;
                self.preview(ctx, file, origin)
            }
            Message::Previewed { path, result } => {
                if let Some(preview) = &mut self.preview
                    && preview.file.path == path
                {
                    preview.image = Some(result);
                }
                Task::none()
            }
            Message::ClosePreview => {
                self.preview = None;
                Task::none()
            }
            Message::NewFolder => {
                let Some(folder) = self.open_folder().map(str::to_owned) else {
                    return Task::none();
                };
                let parent = folder.clone();
                self.prompt(
                    fl!("browse-new-folder-title"),
                    fl!("browse-new-folder-confirm"),
                    "",
                    folder,
                    origin,
                    move |name| Some(Change::CreateDirectory(join_remote_path(&parent, &name))),
                )
            }
            Message::Rename(file) => {
                self.menu = None;
                let Some(parent) = parent_of(&file.path) else {
                    return Task::none();
                };
                let folder = parent.clone();
                let initial = file.name.clone();
                self.prompt(
                    fl!("browse-rename-title"),
                    fl!("browse-rename-confirm"),
                    &initial,
                    folder,
                    origin,
                    move |name| {
                        (name != file.name).then(|| Change::Move {
                            from: file.path.clone(),
                            to: join_remote_path(&parent, &name),
                        })
                    },
                )
            }
            Message::Delete(file) => {
                self.menu = None;
                let (Some(device_id), Some(folder)) = (self.open_device(), parent_of(&file.path))
                else {
                    return Task::none();
                };
                let (title, body) = if file.kind == FileKind::Directory {
                    (
                        fl!("browse-delete-folder-title"),
                        fl!("browse-delete-folder-body", name = file.name.as_str()),
                    )
                } else {
                    (
                        fl!("browse-delete-file-title"),
                        fl!("browse-delete-file-body", name = file.name.as_str()),
                    )
                };
                shell::confirm(
                    origin,
                    title,
                    body,
                    fl!("browse-delete-confirm"),
                    Feature::Browse(Message::Change {
                        device_id,
                        folder,
                        change: Change::Delete(file.path),
                    }),
                )
            }
            Message::Change {
                device_id,
                folder,
                change,
            } => self.change(ctx, device_id, folder, change, origin),
            Message::Changed {
                device_id,
                folder,
                error,
            } => {
                // The folder shows what the change did, whether it worked or
                // not, if it is still held.
                let folder = Some(folder);
                let refetch = if self.open_device().as_deref() == Some(device_id.as_str())
                    && self.listings.contains_key(&folder)
                {
                    self.fetch(ctx, folder, origin)
                } else {
                    Task::none()
                };
                match error {
                    Some(error) => Task::batch([refetch, shell::toast(origin, error)]),
                    None => refetch,
                }
            }
            Message::PickUploads => {
                let (Some(device_id), Some(folder)) =
                    (self.open_device(), self.open_folder().map(str::to_owned))
                else {
                    return Task::none();
                };
                shell::pick_files(
                    origin,
                    fl!(
                        "browse-upload-title",
                        folder = folder_name(&folder, self.roots())
                    ),
                    Arc::new(move |paths| {
                        Feature::Browse(Message::Upload {
                            device_id: device_id.clone(),
                            folder: folder.clone(),
                            paths,
                        })
                    }),
                )
            }
            Message::Upload {
                device_id,
                folder,
                paths,
            } => self.upload(ctx, device_id, folder, paths, origin),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        if self.preview.is_none() {
            return Subscription::none();
        }
        keyboard::listen().filter_map(|event| match event {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            } => Some(Message::ClosePreview),
            _ => None,
        })
    }
}

/// The folder holding `path`; `/` has none.
fn parent_of(path: &str) -> Option<String> {
    split_remote_path(path).map(|(parent, _)| parent.to_owned())
}

/// Why `name` can't name a file, checked before asking the device.
fn invalid_name_reason(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        Some(fl!("browse-name-empty"))
    } else if name == "." || name == ".." {
        Some(fl!("browse-name-reserved"))
    } else if name.contains('/') {
        Some(fl!("browse-name-slash"))
    } else {
        None
    }
}

/// Whether the device's files can be browsed now.
fn shares_files(device: &DeviceSnapshot) -> bool {
    device.paired
        && device.reachability == DeviceReachability::Connected
        && advertises_browse(device)
}

/// Whether the device says it can share its files at all.
fn advertises_browse(device: &DeviceSnapshot) -> bool {
    device
        .incoming_capabilities
        .iter()
        .any(|capability| capability == REQUEST_PACKET_TYPE)
}

#[cfg(test)]
pub(crate) mod tests {
    use iced::{Element, widget};
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::{EventData, testing::handle},
        plugins::browse::BrowseError,
        ui::{
            shell::{PickFiles, Prompt},
            testing,
        },
    };
    use files::tests::{Call, FakeFiles, INTERNAL, SD_CARD, lock, phone_files, pixel};

    /// The page of the test phone's files showing `folder`, or its storage.
    fn files_route(folder: Option<&str>) -> Route {
        Route::Browse {
            device: pixel().device_id,
            folder: folder.map(str::to_owned),
        }
    }

    /// The browse message `feature` holds.
    fn browse(feature: Feature) -> Message {
        let Feature::Browse(message) = feature else {
            panic!("not a browse message: {feature:?}");
        };
        message
    }

    /// The browser, with the shell's part played: navigation reaches
    /// `on_route`, and the other requests are kept to look at.
    pub(super) struct Browser {
        pub(super) ui: BrowseUi,
        pub(super) ctx: UiContext,
        pub(super) files: Arc<FakeFiles>,
        pub(super) route: Route,
        pub(super) requests: Vec<ui::Message>,
    }

    impl Browser {
        pub(super) async fn new() -> Self {
            Self::with_device(pixel()).await
        }

        pub(super) async fn with_device(device: DeviceSnapshot) -> Self {
            let (core, _commands) = handle().await;
            let mut ctx = UiContext::new(core, tokio::runtime::Handle::current());
            *ctx.store_mut() = testing::store("Desk", vec![device]);
            let files = phone_files();
            Self {
                ui: BrowseUi::with_files(files.clone()),
                ctx,
                files,
                route: Route::Devices,
                requests: Vec::new(),
            }
        }

        pub(super) fn device(&self) -> &DeviceSnapshot {
            self.ctx.device(&pixel().device_id).expect("the phone")
        }

        /// Run `task`, its messages and what they lead to.
        pub(super) async fn run(&mut self, task: Task<ui::Message>) {
            let mut pending = vec![task];
            while let Some(task) = pending.pop() {
                for message in testing::outputs(task).await {
                    match message {
                        ui::Message::Feature(Feature::Browse(message), origin) => {
                            pending.push(self.ui.update(&self.ctx, message, origin));
                        }
                        ui::Message::Navigate(route, _) => {
                            self.route = route.clone();
                            pending.push(self.ui.on_route(&self.ctx, &route));
                        }
                        request => self.requests.push(request),
                    }
                }
            }
        }

        pub(super) async fn send(&mut self, message: Message) {
            let task = self.ui.update(&self.ctx, message, Origin::Window);
            self.run(task).await;
        }

        pub(super) async fn go(&mut self, folder: Option<&str>) {
            self.route = files_route(folder);
            let route = self.route.clone();
            let task = self.ui.on_route(&self.ctx, &route);
            self.run(task).await;
        }

        pub(super) fn page(&self) -> Element<'_, Message> {
            let Route::Browse { folder, .. } = &self.route else {
                panic!("not on a browse page: {:?}", self.route);
            };
            self.ui.view(self.device(), folder.clone())
        }

        pub(super) fn shows(&self, text: &str) -> bool {
            Simulator::new(self.page()).find(text).is_ok()
        }

        pub(super) fn top(&self, text: &str) -> f32 {
            let mut ui = Simulator::new(self.page());
            ui.find(text).unwrap().visible_bounds().unwrap().y
        }

        pub(super) async fn click(
            &mut self,
            target: impl iced_test::selector::Selector<
                Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
            > + Send,
        ) {
            let messages: Vec<Message> = {
                let mut ui = Simulator::new(self.page());
                ui.click(target).expect("the target is on the page");
                ui.into_messages().collect()
            };
            for message in messages {
                self.send(message).await;
            }
        }

        /// Open `name`'s actions and choose `action`.
        pub(super) async fn row_action(&mut self, name: &str, action: &str) {
            let folder = self.ui.open_folder().unwrap().to_owned();
            self.send(Message::Menu(join_remote_path(&folder, name)))
                .await;
            self.click(action).await;
        }

        pub(super) fn take_requests(&mut self) -> Vec<ui::Message> {
            std::mem::take(&mut self.requests)
        }
    }

    #[tokio::test]
    async fn storage_then_folders_with_a_breadcrumb_back_up() {
        let mut browser = Browser::new().await;
        browser
            .send(Message::Browse {
                device_id: pixel().device_id,
            })
            .await;
        assert!(browser.shows("Files on Pixel"));
        assert!(browser.shows("All files"));
        assert!(browser.shows("SD card"));

        browser.click("All files").await;
        assert_eq!(browser.route, files_route(Some(INTERNAL)));
        // Folders first; hidden files stay hidden.
        assert!(browser.top("DCIM") < browser.top("notes.txt"));
        assert!(browser.shows("2.0 KB"));
        assert!(browser.shows("Sep 24, 2026, 2:03\u{A0}PM"));
        assert!(!browser.shows(".nomedia"));

        browser.click("DCIM").await;
        browser.click("Camera").await;
        assert!(browser.shows("This folder is empty. Drop files here to upload them."));

        // The breadcrumb names the root as the device does.
        browser.click("All files").await;
        assert!(browser.shows("notes.txt"));
        browser.click(widget::Id::from("Up")).await;
        assert!(browser.shows("SD card"));
        assert_eq!(browser.route, files_route(None));

        browser.click(widget::Id::from("Back")).await;
        assert_eq!(browser.route, Route::Device(pixel().device_id));
        assert!(
            browser.ui.listings.is_empty(),
            "leaving lets go of the files"
        );
    }

    #[tokio::test]
    async fn a_folder_opened_directly_still_names_its_root() {
        let mut browser = Browser::new().await;
        browser.go(Some(&format!("{INTERNAL}/DCIM"))).await;
        assert_eq!(
            browser.files.calls(),
            [
                Call::List(Some(format!("{INTERNAL}/DCIM"))),
                Call::List(None)
            ]
        );
        assert!(browser.shows("All files"));
        assert!(browser.shows("Camera"));
        browser.click(widget::Id::from("Up")).await;
        assert!(browser.shows("notes.txt"));
        // From a root, Up goes to the storage.
        browser.click(widget::Id::from("Up")).await;
        assert!(browser.shows("SD card"));
    }

    #[tokio::test]
    async fn opening_a_file_downloads_it() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        browser.click("notes.txt").await;
        assert!(
            browser
                .files
                .calls()
                .contains(&Call::Download(format!("{INTERNAL}/notes.txt")))
        );
        let [ui::Message::Toast { text, action, .. }] = &browser.take_requests()[..] else {
            panic!("a toast");
        };
        assert_eq!(text, "Downloading notes.txt");
        assert_eq!(action, &Some(("Transfers".into(), Route::Transfers)));
    }

    /// The prompt the browser asked the shell for.
    fn prompt(requests: Vec<ui::Message>) -> Prompt {
        let [ui::Message::Prompt(prompt)] = <[_; 1]>::try_from(requests).unwrap() else {
            panic!("a prompt");
        };
        *prompt
    }

    #[tokio::test]
    async fn renaming_moves_the_file_within_its_folder() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        browser.row_action("notes.txt", "Rename").await;
        let Prompt {
            title,
            initial,
            selection,
            confirm_label,
            then,
            ..
        } = prompt(browser.take_requests());
        assert_eq!(
            (title.as_str(), confirm_label.as_str()),
            ("Rename", "Rename")
        );
        assert_eq!(initial, "notes.txt");
        assert_eq!(selection, Some(0..5), "the name without its extension");

        browser.send(browse(then("todo.txt".into()))).await;
        assert!(browser.files.calls().contains(&Call::Move(
            format!("{INTERNAL}/notes.txt"),
            format!("{INTERNAL}/todo.txt")
        )));
        // The folder is listed again to show the change.
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);

        // The same name changes nothing.
        browser.row_action("notes.txt", "Rename").await;
        let Prompt { then, .. } = prompt(browser.take_requests());
        browser.send(browse(then("notes.txt".into()))).await;
        assert_eq!(
            browser
                .files
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::Move(..)))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_name_with_a_slash_is_refused_before_asking_the_device() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        browser.click(widget::Id::from("New folder")).await;
        let Prompt {
            validate,
            initial,
            selection,
            ..
        } = prompt(browser.take_requests());
        assert_eq!((initial.as_str(), selection), ("", None));
        assert_eq!(validate("a/b").as_deref(), Some("Names can’t contain “/”."));
        assert_eq!(validate(" ").as_deref(), Some("Enter a name."));
        assert_eq!(validate("..").as_deref(), Some("That name is reserved."));
        assert_eq!(validate("Trip"), None);
        assert!(
            !browser
                .files
                .calls()
                .iter()
                .any(|call| matches!(call, Call::CreateDirectory(_)))
        );
    }

    #[tokio::test]
    async fn new_folders_are_created_in_the_open_folder() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        browser.click(widget::Id::from("New folder")).await;
        let Prompt {
            title,
            confirm_label,
            then,
            ..
        } = prompt(browser.take_requests());
        assert_eq!(
            (title.as_str(), confirm_label.as_str()),
            ("New folder", "Create")
        );
        browser.send(browse(then("Trip".into()))).await;
        assert!(
            browser
                .files
                .calls()
                .contains(&Call::CreateDirectory(format!("{INTERNAL}/Trip")))
        );
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);
    }

    #[tokio::test]
    async fn deleting_asks_first_and_warns_about_folder_contents() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        browser.row_action("DCIM", "Delete").await;
        let [
            ui::Message::Confirm {
                title,
                body,
                confirm_label,
                then,
                ..
            },
        ] = &browser.take_requests()[..]
        else {
            panic!("a confirmation");
        };
        assert_eq!(title, "Delete folder?");
        assert!(body.starts_with("“DCIM” and everything in it"), "{body}");
        assert_eq!(confirm_label, "Delete");
        browser.send(browse(then.clone())).await;
        assert!(
            browser
                .files
                .calls()
                .contains(&Call::Delete(format!("{INTERNAL}/DCIM")))
        );

        browser.row_action("notes.txt", "Delete").await;
        let [ui::Message::Confirm { title, body, .. }] = &browser.take_requests()[..] else {
            panic!("a confirmation");
        };
        assert_eq!(title, "Delete file?");
        assert_eq!(
            body,
            "“notes.txt” will be deleted from the device. This can’t be undone."
        );
    }

    #[tokio::test]
    async fn a_failed_change_is_reported_and_the_folder_listed_again() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        *lock(&browser.files.fail_changes) = Some(|| BrowseError::Exists);
        browser.click(widget::Id::from("New folder")).await;
        let Prompt { then, .. } = prompt(browser.take_requests());
        browser.send(browse(then("DCIM".into()))).await;
        let [ui::Message::Toast { text, .. }] = &browser.take_requests()[..] else {
            panic!("a toast");
        };
        assert_eq!(text, "There is already a file or folder with that name.");
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);
    }

    #[tokio::test]
    async fn files_dropped_on_a_folder_are_uploaded_into_it() {
        let mut browser = Browser::new().await;
        let local = tempfile::tempdir().unwrap();
        let photo = local.path().join("photo.jpg");
        std::fs::write(&photo, "jpg").unwrap();
        let gone = local.path().join("gone.jpg");

        browser.go(None).await;
        let storage = browser.route.clone();
        assert!(
            browser.ui.drop_target(browser.device(), &storage).is_none(),
            "the storage isn't a folder: a drop there sends instead"
        );
        browser.go(Some(INTERNAL)).await;
        let here = browser.route.clone();
        let target = browser
            .ui
            .drop_target(browser.device(), &here)
            .expect("the folder takes drops");
        assert_eq!(target.label, "Drop to upload to All files");
        browser
            .send(browse((target.on_drop)(vec![photo.clone()])))
            .await;
        assert!(browser.files.calls().contains(&Call::Upload {
            folder: INTERNAL.into(),
            local: photo.clone(),
        }));
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);
        assert!(browser.take_requests().is_empty(), "nothing to report");

        // Failures are summed up once.
        browser
            .send(browse((target.on_drop)(vec![gone, photo])))
            .await;
        let [ui::Message::Toast { text, .. }] = &browser.take_requests()[..] else {
            panic!("a toast");
        };
        assert_eq!(text, "Couldn’t upload gone.jpg: The file couldn’t be read.");
    }

    #[tokio::test]
    async fn upload_files_picks_files_for_the_open_folder() {
        let mut browser = Browser::new().await;
        browser.go(Some(SD_CARD)).await;
        browser.click(widget::Id::from("Upload files")).await;
        let [ui::Message::PickFiles(PickFiles { title, then, .. })] = &browser.take_requests()[..]
        else {
            panic!("a picker");
        };
        assert_eq!(title, "Upload files to SD card");
        let Message::Upload { folder, paths, .. } = browse(then(vec!["/tmp/a.txt".into()])) else {
            panic!("the picked files are uploaded");
        };
        assert_eq!(folder, SD_CARD);
        assert_eq!(paths, [PathBuf::from("/tmp/a.txt")]);

        // Only inside a folder.
        browser.go(None).await;
        browser.click(widget::Id::from("Upload files")).await;
        browser.click(widget::Id::from("New folder")).await;
        assert!(browser.take_requests().is_empty());
    }

    #[tokio::test]
    async fn a_device_that_refuses_says_why_and_how_to_fix_it() {
        let mut browser = Browser::new().await;
        *lock(&browser.files.refuse) = Some(|| BrowseError::Unavailable {
            reason: Some("No storage locations configured".into()),
        });
        browser.go(None).await;
        let says = "The device isn’t sharing its files (No storage locations configured). In \
                    KDE Connect on the device, allow access to files in the Filesystem expose \
                    plugin.";
        assert!(browser.shows(says));

        *lock(&browser.files.refuse) = None;
        browser.click("Retry").await;
        assert!(browser.shows("All files"));
    }

    #[tokio::test]
    async fn browsing_needs_a_device_that_shares_its_files() {
        let browse = |device: &DeviceSnapshot| {
            let [action] = device_actions(device).try_into().unwrap();
            action
        };
        let action = browse(&pixel());
        assert_eq!(action.label, "Browse files");
        assert!(action.enabled);
        assert!(action.visible_in_tray);
        let incapable = browse(&testing::device("Pixel"));
        assert!(!incapable.enabled, "listed, but disabled");
        assert!(!incapable.visible_in_tray, "and not in the tray");

        let mut away = pixel();
        away.reachability = DeviceReachability::Discovered;
        assert!(!browse(&away).enabled);

        let mut browser = Browser::with_device(away).await;
        browser.go(None).await;
        assert!(browser.shows("Connect Pixel to browse its files."));
        assert!(browser.files.calls().is_empty(), "nothing asked");

        let mut browser = Browser::with_device(testing::device("Pixel")).await;
        browser.go(None).await;
        assert!(browser.shows("Pixel doesn’t share its files."));
    }

    #[tokio::test]
    async fn the_files_are_listed_again_once_the_device_is_back() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        let event = |device: DeviceSnapshot| CoreEvent {
            sequence: 1,
            timestamp: 1,
            event: EventData::DeviceUpdated(device),
        };
        let mut away = pixel();
        away.reachability = DeviceReachability::Discovered;
        for device in [away, pixel()] {
            let event = event(device);
            browser.ctx.store_mut().apply_event(&event.event);
            let task = browser.ui.on_event(&browser.ctx, &event);
            browser.run(task).await;
        }
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);
        assert_eq!(browser.files.listed(None), 2);

        // Nothing changed: nothing listed.
        let event = event(pixel());
        let task = browser.ui.on_event(&browser.ctx, &event);
        browser.run(task).await;
        assert_eq!(browser.files.listed(Some(INTERNAL)), 2);
    }

    #[tokio::test]
    async fn only_the_newest_listing_of_a_folder_counts() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        let stale = Message::Listed {
            device_id: pixel().device_id,
            folder: Some(INTERNAL.into()),
            request: 0,
            result: Err("stale".into()),
        };
        browser.send(stale).await;
        assert!(browser.shows("notes.txt"));
    }
}

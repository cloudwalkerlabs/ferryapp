//! The browser's page: the breadcrumbs, the listing with its sortable
//! columns, and each file's row and actions.

use std::cmp::Ordering;

use iced::{
    Alignment, Background, Border, Element, Length, Padding, Theme,
    widget::{Space, button, column, container, responsive, row, rule, scrollable, text},
};
use iced_fonts::lucide;

use super::{
    BrowseUi, Column, Message, Sort,
    preview::{can_preview, extension, preview_view},
    shares_files,
};
use crate::{
    core::{DeviceReachability, DeviceSnapshot},
    plugins::browse::{FileEntry, FileKind, files::join_remote_path},
    ui::{
        i18n::fl,
        overlay::dialog,
        widgets::{self, HeaderAction, Icon, bold, format_bytes, format_timestamp},
    },
};

/// From this width the listing shows when files were modified.
const WIDE: f32 = 600.0;
const SIZE_WIDTH: f32 = 96.0;
const MODIFIED_WIDTH: f32 = 168.0;
const ICON_WIDTH: f32 = 32.0;
const MORE_WIDTH: f32 = 40.0;

impl BrowseUi {
    /// The page of `device`'s files showing `folder`, or its storage, with
    /// the image preview over it.
    pub fn view<'a>(
        &'a self,
        device: &'a DeviceSnapshot,
        folder: Option<String>,
    ) -> Element<'a, Message> {
        let page = self.page(device, folder);
        match &self.preview {
            Some(preview) => {
                dialog::modal(page, preview_view(preview), Some(Message::ClosePreview))
            }
            None => page,
        }
    }

    fn page<'a>(
        &'a self,
        device: &'a DeviceSnapshot,
        folder: Option<String>,
    ) -> Element<'a, Message> {
        let available = shares_files(device);
        let in_folder = folder.is_some();
        let when = |enabled: bool, message: Message| enabled.then_some(message);
        let actions = vec![
            HeaderAction {
                icon: lucide::file_up,
                tooltip: fl!("browse-upload"),
                on_press: when(available && in_folder, Message::PickUploads),
            },
            HeaderAction {
                icon: lucide::folder_plus,
                tooltip: fl!("browse-new-folder"),
                on_press: when(available && in_folder, Message::NewFolder),
            },
            HeaderAction {
                icon: lucide::refresh_cw,
                tooltip: fl!("browse-refresh"),
                on_press: when(available, Message::Refresh),
            },
            if self.show_hidden {
                HeaderAction::new(
                    lucide::eye_off,
                    fl!("browse-hide-hidden"),
                    Message::ToggleHidden,
                )
            } else {
                HeaderAction::new(
                    lucide::eye,
                    fl!("browse-show-hidden"),
                    Message::ToggleHidden,
                )
            },
        ];
        let header = widgets::page_header(
            fl!("browse-title", name = device.device_name.as_str()),
            Some(Message::Back),
            actions,
        );
        let body: Element<'a, Message> = if !available {
            let reason = if device.reachability == DeviceReachability::Connected {
                fl!("browse-not-shared", name = device.device_name.as_str())
            } else {
                fl!("browse-not-connected", name = device.device_name.as_str())
            };
            widgets::error_view(reason, None)
        } else {
            column![
                breadcrumbs(folder.as_deref(), self.roots()),
                rule::horizontal(1),
                self.listing(folder),
            ]
            .spacing(4)
            .into()
        };
        widgets::page(header, body)
    }

    fn listing<'a>(&'a self, folder: Option<String>) -> Element<'a, Message> {
        let in_folder = folder.is_some();
        let answer = self
            .listings
            .get(&folder)
            .and_then(|listing| listing.answer.as_ref());
        let listing = match answer {
            None if in_folder => return widgets::loading(fl!("browse-loading-folder")),
            None => return widgets::loading(fl!("browse-connecting")),
            Some(Err(error)) => return widgets::error_view(error.as_str(), Some(Message::Refresh)),
            Some(Ok(listing)) => listing,
        };
        let entries: Vec<&FileEntry> = listing
            .entries
            .iter()
            .filter(|entry| self.show_hidden || !entry.name.starts_with('.'))
            .collect();
        if !in_folder {
            if entries.is_empty() {
                return widgets::empty_state(
                    lucide::hard_drive,
                    fl!("browse-no-storage"),
                    None,
                    None,
                );
            }
            let rows = entries.into_iter().map(|root| {
                button(
                    row![
                        lucide::hard_drive().size(20).style(text::secondary),
                        column![
                            text(&root.name),
                            text(&root.path).size(13).style(text::secondary),
                        ]
                        .spacing(2),
                    ]
                    .spacing(14)
                    .align_y(Alignment::Center),
                )
                .padding([12, 14])
                .width(Length::Fill)
                .style(widgets::card_button)
                .on_press(Message::Open(Some(root.path.clone())))
                .into()
            });
            return scrollable(column(rows).spacing(8).padding([8, 0]))
                .spacing(6)
                .height(Length::Fill)
                .into();
        }
        let sort = self.sort;
        let mut entries = entries;
        entries.sort_by(|a, b| compare(a, b, sort));
        responsive(move |size| {
            let wide = size.width >= WIDE;
            let list: Element<'a, Message> = if entries.is_empty() {
                widgets::empty_state(lucide::folder_open, fl!("browse-empty-folder"), None, None)
            } else {
                scrollable(column(entries.iter().map(|file| self.file_row(file, wide))).spacing(2))
                    .spacing(6)
                    .height(Length::Fill)
                    .into()
            };
            column![header_row(wide, sort), rule::horizontal(1), list]
                .spacing(2)
                .into()
        })
        .into()
    }

    fn file_row<'a>(&'a self, file: &'a FileEntry, wide: bool) -> Element<'a, Message> {
        let mut line = row![
            container(file_icon(file)().size(18).style(text::secondary)).width(ICON_WIDTH),
            container(text(&file.name).wrapping(text::Wrapping::None))
                .width(Length::Fill)
                .clip(true),
            text(file.size.map(format_bytes).unwrap_or_default())
                .size(13)
                .style(text::secondary)
                .width(SIZE_WIDTH)
                .align_x(Alignment::End),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if wide {
            line = line.push(
                text(file.modified_at.map(format_timestamp).unwrap_or_default())
                    .size(13)
                    .style(text::secondary)
                    .width(MODIFIED_WIDTH)
                    .align_x(Alignment::End),
            );
        }
        line = line.push(
            container(widgets::icon_button(
                lucide::ellipsis_vertical,
                fl!("browse-more"),
                Some(Message::Menu(file.path.clone())),
            ))
            .width(MORE_WIDTH)
            .align_x(Alignment::End),
        );
        let row_button = button(line)
            .padding([2, 8])
            .width(Length::Fill)
            .style(row_style)
            .on_press(Message::Activate(file.clone()));
        if self.menu.as_deref() != Some(file.path.as_str()) {
            return row_button.into();
        }

        let mut actions = row![].spacing(8);
        if can_preview(file) {
            actions = actions.push(menu_button(
                lucide::eye,
                fl!("browse-preview"),
                Message::Preview(file.clone()),
            ));
        }
        if file.kind != FileKind::Directory {
            actions = actions.push(menu_button(
                lucide::download,
                fl!("browse-download"),
                Message::Download(file.clone()),
            ));
        }
        actions = actions
            .push(menu_button(
                lucide::pencil,
                fl!("browse-rename"),
                Message::Rename(file.clone()),
            ))
            .push(menu_button(
                lucide::trash_two,
                fl!("browse-delete"),
                Message::Delete(file.clone()),
            ));
        column![
            row_button,
            container(actions.wrap().vertical_spacing(8)).padding(Padding {
                // Under the name, past the row's icon.
                left: 8.0 + ICON_WIDTH + 8.0,
                ..Padding::from([4, 8])
            })
        ]
        .spacing(2)
        .into()
    }
}

/// The page's path from the storage: "Storage", the root as the device
/// names it, then each folder, each a link back up, and Up before them.
fn breadcrumbs<'a>(folder: Option<&str>, roots: &[FileEntry]) -> Element<'a, Message> {
    let crumbs = crumbs(folder, roots);
    let last = crumbs.len() - 1;
    let mut trail = row![].spacing(2).align_y(Alignment::Center);
    for (index, (label, target)) in crumbs.into_iter().enumerate() {
        if index > 0 {
            trail = trail.push(lucide::chevron_right().size(14).style(text::secondary));
        }
        trail = trail.push(if index == last {
            container(text(label).font(bold()).wrapping(text::Wrapping::None))
                .padding([6, 12])
                .into()
        } else {
            widgets::link_button(label, Message::Open(target))
        });
    }
    row![
        widgets::icon_button(
            lucide::arrow_up,
            fl!("browse-up"),
            folder.map(|_| Message::Up)
        ),
        // Deep paths scroll, showing their end.
        scrollable(trail)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(3).scroller_width(3),
            ))
            .anchor_right()
            .width(Length::Fill),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

/// Each breadcrumb's label and the folder it opens (`None`: the storage).
fn crumbs(folder: Option<&str>, roots: &[FileEntry]) -> Vec<(String, Option<String>)> {
    let mut crumbs = vec![(fl!("browse-storage"), None)];
    let Some(folder) = folder else {
        return crumbs;
    };
    let root = root_of(folder, roots);
    let mut current = root.map(|root| root.path.clone()).unwrap_or_default();
    if let Some(root) = root {
        crumbs.push((root.name.clone(), Some(root.path.clone())));
    }
    let rest = match root {
        Some(root) => &folder[root.path.len()..],
        None => folder,
    };
    for segment in rest.split('/').filter(|segment| !segment.is_empty()) {
        current = join_remote_path(if current.is_empty() { "/" } else { &current }, segment);
        crumbs.push((segment.to_owned(), Some(current.clone())));
    }
    crumbs
}

/// The folder's name as the breadcrumbs end with it.
pub(super) fn folder_name(folder: &str, roots: &[FileEntry]) -> String {
    crumbs(Some(folder), roots)
        .pop()
        .map(|(label, _)| label)
        .unwrap_or_default()
}

fn header_row<'a>(wide: bool, sort: Sort) -> Element<'a, Message> {
    let column_button = |label: String, by: Column| -> Element<'a, Message> {
        let mut content = row![text(label).size(13).font(bold())]
            .spacing(4)
            .align_y(Alignment::Center);
        if sort.by == by {
            let arrow = if sort.ascending {
                lucide::arrow_up()
            } else {
                lucide::arrow_down()
            };
            content = content.push(arrow.size(13));
        }
        button(content)
            .padding([6, 0])
            .style(row_style)
            .on_press(Message::Sort(by))
            .into()
    };
    let mut header = row![
        Space::new().width(ICON_WIDTH),
        container(column_button(fl!("browse-column-name"), Column::Name)).width(Length::Fill),
        container(column_button(fl!("browse-column-size"), Column::Size))
            .width(SIZE_WIDTH)
            .align_x(Alignment::End),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if wide {
        header = header.push(
            container(column_button(
                fl!("browse-column-modified"),
                Column::Modified,
            ))
            .width(MODIFIED_WIDTH)
            .align_x(Alignment::End),
        );
    }
    container(header.push(Space::new().width(MORE_WIDTH)))
        .padding([0, 8])
        .into()
}

/// A plain row that shows hover and press.
fn row_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let background = match status {
        button::Status::Hovered => Some(palette.background.weak.color),
        button::Status::Pressed => Some(palette.background.strong.color),
        _ => None,
    };
    button::Style {
        background: background.map(Background::Color),
        text_color: palette.background.base.text,
        border: Border::default().rounded(8),
        ..button::Style::default()
    }
}

fn menu_button<'a>(icon: Icon, label: String, message: Message) -> Element<'a, Message> {
    button(
        row![icon().size(14), text(label).size(13)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .style(widgets::tonal)
    .on_press(message)
    .into()
}

/// Folders first, then by the chosen column, then by name.
fn compare(a: &FileEntry, b: &FileEntry, sort: Sort) -> Ordering {
    let (a_folder, b_folder) = (a.kind == FileKind::Directory, b.kind == FileKind::Directory);
    if a_folder != b_folder {
        return b_folder.cmp(&a_folder);
    }
    let by_name = a.name.to_lowercase().cmp(&b.name.to_lowercase());
    let order = match sort.by {
        Column::Name => by_name,
        Column::Size => a.size.unwrap_or(0).cmp(&b.size.unwrap_or(0)),
        Column::Modified => a.modified_at.unwrap_or(0).cmp(&b.modified_at.unwrap_or(0)),
    }
    .then(by_name);
    if sort.ascending {
        order
    } else {
        order.reverse()
    }
}

fn file_icon(file: &FileEntry) -> Icon {
    if file.kind == FileKind::Directory {
        return lucide::folder;
    }
    match extension(&file.name).as_deref().unwrap_or("") {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" => lucide::image,
        "mp4" | "mkv" | "mov" | "webm" | "3gp" => lucide::film,
        "mp3" | "m4a" | "ogg" | "opus" | "flac" | "wav" => lucide::music,
        "pdf" => lucide::file_text,
        "zip" | "tar" | "gz" | "7z" | "rar" => lucide::file_archive,
        "apk" => lucide::package,
        _ => lucide::file,
    }
}

/// The storage root that `path` is in, among `roots`.
fn root_of<'a>(path: &str, roots: &'a [FileEntry]) -> Option<&'a FileEntry> {
    roots.iter().find(|root| {
        path == root.path
            || path
                .strip_prefix(&root.path)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use iced::widget;
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::ui::{
        features::browse::{
            files::tests::{INTERNAL, directory, lock},
            tests::Browser,
        },
        testing,
    };

    #[test]
    fn crumbs_follow_the_root_then_each_folder() {
        let roots = [directory(INTERNAL, Some("All files"))];
        assert_eq!(
            crumbs(Some(&format!("{INTERNAL}/DCIM/Camera")), &roots),
            [
                ("Storage".to_owned(), None),
                ("All files".into(), Some(INTERNAL.into())),
                ("DCIM".into(), Some(format!("{INTERNAL}/DCIM"))),
                ("Camera".into(), Some(format!("{INTERNAL}/DCIM/Camera"))),
            ]
        );
        // Outside every root, the path's own folders.
        assert_eq!(
            crumbs(Some("/sdcard/Music"), &roots),
            [
                ("Storage".to_owned(), None),
                ("sdcard".into(), Some("/sdcard".into())),
                ("Music".into(), Some("/sdcard/Music".into())),
            ]
        );
        // A root's name isn't mistaken for a prefix of another folder.
        assert_eq!(
            folder_name(&format!("{INTERNAL}0/x"), &roots),
            "x",
            "{INTERNAL}0 isn't inside {INTERNAL}"
        );
    }

    #[tokio::test]
    async fn hidden_files_can_be_shown() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        assert!(!browser.shows(".nomedia"));
        browser.click(widget::Id::from("Show hidden files")).await;
        assert!(browser.shows(".nomedia"));
        browser.click(widget::Id::from("Hide hidden files")).await;
        assert!(!browser.shows(".nomedia"));
    }

    #[tokio::test]
    async fn columns_sort_both_ways_with_folders_first() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        let order = |browser: &Browser| {
            let mut names = ["DCIM", "notes.txt", "photo.png"];
            names.sort_by(|a, b| browser.top(a).total_cmp(&browser.top(b)));
            names
        };
        assert_eq!(order(&browser), ["DCIM", "notes.txt", "photo.png"]);
        browser.click("Size").await;
        assert_eq!(order(&browser), ["DCIM", "photo.png", "notes.txt"]);
        browser.click("Size").await;
        assert_eq!(order(&browser), ["DCIM", "notes.txt", "photo.png"]);
        browser.click("Name").await;
        browser.click("Name").await;
        assert_eq!(order(&browser), ["DCIM", "photo.png", "notes.txt"]);
    }

    #[tokio::test]
    async fn modified_shows_only_on_a_wide_window() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        let narrow = Simulator::with_size(Default::default(), (500.0, 600.0), browser.page())
            .find("Sep 24, 2026, 2:03\u{A0}PM")
            .is_ok();
        assert!(!narrow);
        assert!(browser.shows("Modified"), "the default size is wide");
    }

    #[tokio::test]
    async fn snapshot_files() {
        let mut browser = Browser::new().await;
        browser.go(None).await;
        testing::snapshot("files-storage", (720.0, 420.0), || browser.page());
        browser.go(Some(&format!("{INTERNAL}/DCIM/Camera"))).await;
        testing::snapshot("files-empty", (720.0, 420.0), || browser.page());
        browser.go(Some(INTERNAL)).await;
        testing::snapshot("files-folder", (720.0, 420.0), || browser.page());
        browser
            .send(Message::Menu(format!("{INTERNAL}/photo.png")))
            .await;
        testing::snapshot("files-folder-narrow", (440.0, 420.0), || browser.page());

        let mut png = std::io::Cursor::new(Vec::new());
        ::image::RgbaImage::from_fn(64, 48, |x, y| {
            ::image::Rgba([(x * 4) as u8, (y * 5) as u8, 160, 255])
        })
        .write_to(&mut png, ::image::ImageFormat::Png)
        .unwrap();
        *lock(&browser.files.content) = png.into_inner();
        browser.click("photo.png").await;
        testing::snapshot("files-preview", (720.0, 520.0), || browser.page());

        let mut browser = Browser::with_device(testing::device("Pixel")).await;
        browser.go(None).await;
        testing::snapshot("files-not-shared", (440.0, 320.0), || browser.page());
    }
}

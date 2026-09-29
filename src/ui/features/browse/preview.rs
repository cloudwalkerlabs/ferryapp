//! Small images open in a preview over the page instead of downloading.

use iced::{
    Alignment, Element, Length, Task,
    widget::{column, container, image, row, text},
};
use iced_fonts::lucide;

use super::{BrowseUi, Message, describe::describe_error, to_app};
use crate::{
    plugins::browse::{FileEntry, FileKind},
    ui::{
        self, Origin,
        context::UiContext,
        i18n::fl,
        overlay::dialog,
        widgets::{self, bold},
    },
};

/// Images opened as a preview rather than downloaded, up to
/// [`MAX_PREVIEW_BYTES`].
const PREVIEW_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];
const MAX_PREVIEW_BYTES: u64 = 32 * 1024 * 1024;

/// An image from the device, shown over the page.
pub(super) struct Preview {
    pub(super) file: FileEntry,
    /// The decoded image, or why it can't be shown; `None` while loading.
    pub(super) image: Option<Result<image::Handle, String>>,
}

impl BrowseUi {
    pub(super) fn preview(
        &mut self,
        ctx: &UiContext,
        file: FileEntry,
        origin: Origin,
    ) -> Task<ui::Message> {
        let Some(device_id) = self.open_device() else {
            return Task::none();
        };
        let path = file.path.clone();
        let read = self.files.clone().read(
            ctx.plugin_context(),
            device_id,
            path.clone(),
            MAX_PREVIEW_BYTES,
        );
        self.preview = Some(Preview { file, image: None });
        ctx.spawn(
            async move {
                let bytes = read.await.map_err(|error| describe_error(&error))?;
                tokio::task::spawn_blocking(move || decode(&bytes))
                    .await
                    .unwrap_or_else(|_| Err(cant_show()))
            },
            move |result| to_app(Message::Previewed { path, result }, origin),
        )
    }
}

pub(super) fn preview_view(preview: &Preview) -> Element<'_, Message> {
    let body: Element<'_, Message> = match &preview.image {
        None => widgets::loading(fl!("browse-loading-image")),
        Some(Err(error)) => widgets::error_view(error.as_str(), None),
        Some(Ok(handle)) => image::viewer(handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
    };
    let card = container(
        column![
            row![
                container(
                    text(&preview.file.name)
                        .size(18)
                        .font(bold())
                        .wrapping(text::Wrapping::None)
                )
                .width(Length::Fill)
                .clip(true),
                widgets::icon_button(
                    lucide::x,
                    fl!("browse-close-preview"),
                    Some(Message::ClosePreview)
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            container(body).height(Length::Fill),
        ]
        .spacing(12),
    )
    .padding(16)
    .width(Length::Fill)
    .height(Length::Fill)
    .max_width(960)
    .max_height(720)
    .style(dialog::surface_style);
    container(card).padding(32).into()
}

/// What an image that can't be decoded says.
fn cant_show() -> String {
    fl!("browse-image-unreadable")
}

/// Decode an image for the preview. iced would decode it only when drawn,
/// and drop the error.
fn decode(bytes: &[u8]) -> Result<image::Handle, String> {
    let decoded = ::image::load_from_memory(bytes).map_err(|_| cant_show())?;
    let rgba = decoded.into_rgba8();
    Ok(image::Handle::from_rgba(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
    ))
}

/// Whether a click on `file` previews it rather than downloading it.
pub(super) fn can_preview(file: &FileEntry) -> bool {
    file.kind != FileKind::Directory
        && extension(&file.name)
            .is_some_and(|extension| PREVIEW_EXTENSIONS.contains(&extension.as_str()))
        && file.size.unwrap_or(0) <= MAX_PREVIEW_BYTES
}

/// A file's extension, lowercase; a leading dot doesn't start one.
pub(super) fn extension(name: &str) -> Option<String> {
    match name.rfind('.') {
        Some(dot) if dot > 0 => Some(name[dot + 1..].to_lowercase()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use iced::widget;

    use super::*;
    use crate::ui::features::browse::{
        files::tests::{Call, INTERNAL, directory, file, lock},
        tests::Browser,
    };

    #[tokio::test]
    async fn opening_a_small_image_previews_it() {
        let mut browser = Browser::new().await;
        browser.go(Some(INTERNAL)).await;
        let mut png = std::io::Cursor::new(Vec::new());
        ::image::RgbaImage::new(4, 3)
            .write_to(&mut png, ::image::ImageFormat::Png)
            .unwrap();
        *lock(&browser.files.content) = png.into_inner();

        browser.click("photo.png").await;
        assert!(
            browser
                .files
                .calls()
                .contains(&Call::Read(format!("{INTERNAL}/photo.png")))
        );
        let preview = browser.ui.preview.as_ref().expect("a preview");
        assert!(matches!(preview.image, Some(Ok(_))), "decoded");
        browser.click(widget::Id::from("Close")).await;
        assert!(browser.ui.preview.is_none());

        // Not an image after all.
        *lock(&browser.files.content) = b"not a png".to_vec();
        browser.row_action("photo.png", "Preview").await;
        assert!(browser.shows(&cant_show()));
        // Large images download instead.
        let mut large = file(&format!("{INTERNAL}/huge.jpg"), MAX_PREVIEW_BYTES + 1);
        assert!(!can_preview(&large));
        large.size = Some(MAX_PREVIEW_BYTES);
        assert!(can_preview(&large));
        assert!(!can_preview(&directory("/x.png", None)));
        assert!(!can_preview(&file("/.png", 1)));
    }
}

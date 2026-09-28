//! The prompt for an incoming pairing request, over every page while one
//! waits for the user.
//!
//! It is drawn from the store's pending requests rather than opened like a
//! dialog, so it goes away by itself when the request is resolved anywhere
//! else: the CLI, a timeout, the peer cancelling.

use iced::{
    Alignment, Element,
    widget::{button, column, container, row, space, text},
};
use uuid::Uuid;

use crate::{
    core::PairingSnapshot,
    ui::{i18n::fl, overlay::dialog::surface, widgets},
};

/// Accept or reject a request, by id.
pub struct Actions<M> {
    pub accept: fn(Uuid) -> M,
    pub reject: fn(Uuid) -> M,
}

/// The prompt for the oldest of `pending` (oldest first), if any. While
/// `busy`, its buttons are disabled; `error` is why the last answer failed.
pub fn view<'a, M: Clone + 'a>(
    pending: &[&'a PairingSnapshot],
    busy: bool,
    error: Option<&'a str>,
    actions: &Actions<M>,
) -> Option<Element<'a, M>> {
    let (pairing, queued) = pending.split_first()?;
    let mut content = column![
        text(fl!("incoming-title")).size(20).font(widgets::bold()),
        text(fl!("incoming-body", name = pairing.device_name.as_str()))
            .size(14)
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(12);
    if let Some(code) = &pairing.verification_code {
        content =
            content.push(container(widgets::verification_code(code)).center_x(iced::Length::Fill));
    }
    if !queued.is_empty() {
        content = content.push(
            text(fl!("incoming-queued", count = queued.len()))
                .size(12)
                .style(text::secondary),
        );
    }
    if let Some(error) = error {
        content = content.push(text(error).size(13).style(text::danger));
    }
    let answer = |label, style: fn(&iced::Theme, button::Status) -> button::Style, message| {
        button(text(label))
            .padding([8, 18])
            .style(style)
            .on_press_maybe((!busy).then_some(message))
    };
    content = content.push(
        container(
            row![
                space::horizontal(),
                answer(
                    fl!("incoming-reject"),
                    button::text,
                    (actions.reject)(pairing.id)
                ),
                answer(
                    fl!("incoming-accept"),
                    widgets::filled,
                    (actions.accept)(pairing.id)
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(iced::Padding::default().top(12)),
    );
    Some(surface(content))
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::{PairingDirection, PairingStatus},
        ui::{overlay::dialog::modal, testing},
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Asked {
        Accept(Uuid),
        Reject(Uuid),
    }

    const ACTIONS: Actions<Asked> = Actions {
        accept: Asked::Accept,
        reject: Asked::Reject,
    };

    fn request(id: u128, name: &str) -> PairingSnapshot {
        PairingSnapshot {
            id: Uuid::from_u128(id),
            device_id: "740bd4b9b4184ee497d6caf1da8151be".into(),
            device_name: name.into(),
            direction: PairingDirection::Incoming,
            status: PairingStatus::AwaitingConfirmation,
            verification_code: Some("9F3A7C21".into()),
            created_at: id as u64,
            expires_at: 30_000,
            error_code: None,
        }
    }

    #[test]
    fn nothing_shows_without_a_request() {
        assert!(view::<Asked>(&[], false, None, &ACTIONS).is_none());
    }

    #[test]
    fn asks_about_the_oldest_request_and_counts_the_rest() {
        let (first, second, third) = (request(1, "Pixel 8a"), request(2, "Tab"), request(3, "TV"));
        let pending = [&first, &second, &third];
        let mut ui = Simulator::new(view(&pending, false, None, &ACTIONS).unwrap());
        for shown in [
            "Pairing request",
            "Pixel 8a wants to pair with this computer. Accept only if it shows the same code:",
            "9F3A7C21",
            "2 more requests waiting",
        ] {
            assert!(ui.find(shown).is_ok(), "{shown} is shown");
        }
        ui.click("Accept").unwrap();
        ui.click("Reject").unwrap();
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            [Asked::Accept(first.id), Asked::Reject(first.id)]
        );
    }

    #[test]
    fn busy_disables_the_answers_and_an_error_shows() {
        let first = request(1, "Pixel 8a");
        let pending = [&first];
        let mut ui = Simulator::new(
            view(
                &pending,
                true,
                Some("The device is not connected right now."),
                &ACTIONS,
            )
            .unwrap(),
        );
        assert!(ui.find("The device is not connected right now.").is_ok());
        assert!(ui.find("1 more request waiting").is_err());
        ui.click("Accept").unwrap();
        assert!(ui.into_messages().next().is_none());
    }

    /// The successful incoming request used in the documentation gallery.
    #[test]
    fn snapshot_pairing_request() {
        let first = request(1, "Pixel 8a");
        let pending = [&first];
        let store = testing::store("Demo desktop", vec![testing::device("Pixel 8a")]);
        testing::snapshot("pairing-request", (440.0, 620.0), || {
            modal(
                crate::ui::pages::devices::view(
                    &store,
                    &|_| Vec::new(),
                    |_| Asked::Reject(first.id),
                    Asked::Reject(first.id),
                ),
                view(&pending, false, None, &ACTIONS).unwrap(),
                None,
            )
        });
    }

    #[test]
    fn snapshot_incoming_prompt() {
        let (first, second) = (request(1, "Pixel 8a"), request(2, "Tab"));
        let pending = [&first, &second];
        testing::snapshot("incoming-pairing", (440.0, 520.0), || {
            modal(
                container(text("The page underneath"))
                    .center(iced::Length::Fill)
                    .into(),
                view(
                    &pending,
                    false,
                    Some("The device is not connected right now."),
                    &ACTIONS,
                )
                .unwrap(),
                None,
            )
        });
    }
}
